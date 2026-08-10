// Config: CLI flags + env vars + config files. No clap, no toml — hand-rolled.
// Precedence: CLI > env > settings.local.json > settings.json
//   > ~/.config/settings.json > managed-settings.json > managed-settings.d/*.json
// Arrays concatenate+deduplicate; objects deep merge; null means delete.
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub enum Approval {
    Never,       // auto-approve everything (trust the model fully)
    Destructive, // ask only for bash + write_file + edit (default)
    Always,      // ask for every tool call
}

impl Approval {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "never" | "off" | "none" | "auto" => Approval::Never,
            "always" | "all" | "y" => Approval::Always,
            _ => Approval::Destructive,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Approval::Never => "never",
            Approval::Destructive => "destructive",
            Approval::Always => "always",
        }
    }
}

/// Permission rule: per-tool, per-content matching with allow/deny/ask behavior.
#[derive(Clone, Debug)]
pub struct PermissionRule {
    pub tool_name: String,
    pub rule_content: String,
    pub behavior: PermissionBehavior,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

impl PermissionBehavior {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "allow" | "yes" | "true" => PermissionBehavior::Allow,
            "deny" | "no" | "false" => PermissionBehavior::Deny,
            _ => PermissionBehavior::Ask,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            PermissionBehavior::Allow => "allow",
            PermissionBehavior::Deny => "deny",
            PermissionBehavior::Ask => "ask",
        }
    }
}

/// Parse a rule string like "Bash(npm test)" or "Edit(//src/**)" into PermissionRule.
/// The format is: ToolName(ruleContent).
pub fn parse_permission_rule(s: &str, behavior: PermissionBehavior) -> Option<PermissionRule> {
    let s = s.trim();
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let tool_name = s[..open].to_string();
    let rule_content = s[open + 1..close].to_string();
    if tool_name.is_empty() {
        return None;
    }
    Some(PermissionRule {
        tool_name,
        rule_content,
        behavior,
    })
}

/**
 * Sandbox execution mode.
 *
 * Replaces the legacy Firejail (Linux) / Seatbelt (macOS `sandbox-exec`) /
 * `unshare -n` trio with a single Microsandbox microVM backend that runs on
 * Linux/KVM, Apple-Silicon macOS, and Windows/WHP. The user never installs
 * Docker, Podman, Firejail, WSL, the `msb` CLI, or a persistent daemon — the
 * embedded SDK downloads its own runtime on first use.
 */
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sandbox {
    /// No sandboxing: agent-controlled workloads run directly on the host
    /// (denylist + approval gate still apply). Selected only when the user
    /// explicitly disables sandboxing.
    None,
    /// Run agent-controlled workloads inside a Microsandbox microVM. The
    /// microVM is created lazily on the first workload and reused for the
    /// session so package installs / build caches persist.
    Microsandbox,
}

impl Sandbox {
    /// Parse a sandbox setting string. Aliases:
    ///   none, off, false, disabled        -> None
    ///   microsandbox, msb, on, true, enabled -> Microsandbox
    /// Legacy values (migrated, not silently downgraded to None):
    ///   firejail, fj, seatbelt, macos, sandbox-exec -> Microsandbox
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "microsandbox" | "msb" | "on" | "true" | "enabled" | "enable" => Sandbox::Microsandbox,
            "firejail" | "fj" | "seatbelt" | "macos" | "sandbox-exec" => Sandbox::Microsandbox,
            _ => Sandbox::None,
        }
    }
    /// Whether `s` is a legacy backend alias (firejail/seatbelt) that we migrate
    /// to `Microsandbox`. Returns the legacy backend name for the deprecation
    /// notice, or `None` for current/`none` values.
    pub fn legacy_alias(s: &str) -> Option<&'static str> {
        match s.to_ascii_lowercase().as_str() {
            "firejail" | "fj" => Some("firejail"),
            "seatbelt" | "macos" | "sandbox-exec" => Some("seatbelt"),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Sandbox::None => "none",
            Sandbox::Microsandbox => "microsandbox",
        }
    }
    pub fn is_enabled(self) -> bool {
        matches!(self, Sandbox::Microsandbox)
    }
}

/// Parse a sandbox setting, emitting a deprecation notice when the user passed
/// a legacy (firejail/seatbelt) value. Returns the resolved mode. The legacy
/// value is preserved as an *intention to enable sandboxing* — it is never
/// silently converted to `none`.
pub fn parse_sandbox_setting(raw: &str) -> Sandbox {
    if let Some(legacy) = Sandbox::legacy_alias(raw) {
        eprintln!(
            "[catalyst-code] deprecation: the '{legacy}' sandbox backend has been replaced by \
             Microsandbox. Continuing with sandbox=microsandbox. Update your config to \
             \"sandbox\": \"microsandbox\" to silence this notice."
        );
    }
    Sandbox::parse(raw)
}

/// Network egress policy for the Microsandbox guest. Translates the legacy
/// `--no-network` flag (now `None`) and adds restrictive modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxNetworkMode {
    /// No network interface at all (guest cannot reach anything). This is the
    /// mode used when `--no-network` / `CATALYST_CODE_NO_NETWORK` is set.
    None,
    /// Default: network up, but cloud-metadata addresses, host-only services,
    /// and (by default) private network ranges are blocked. Public package
    /// registries and source hosts are permitted.
    Restricted,
    /// Network up; only hosts/CIDRs in `sandbox_network_allowlist` are
    /// reachable.
    Allowlist,
}

impl SandboxNetworkMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "allowlist" | "allow" => SandboxNetworkMode::Allowlist,
            "none" | "off" | "disabled" => SandboxNetworkMode::None,
            _ => SandboxNetworkMode::Restricted,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxNetworkMode::None => "none",
            SandboxNetworkMode::Restricted => "restricted",
            SandboxNetworkMode::Allowlist => "allowlist",
        }
    }
    pub fn from_no_network(no_network: bool) -> Self {
        if no_network {
            SandboxNetworkMode::None
        } else {
            SandboxNetworkMode::Restricted
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub base_url: String,
    pub workspace: PathBuf,
    pub approval: Approval,
    pub bash_timeout_secs: u64,
    pub diag_timeout_secs: u64, // wall-clock timeout for the diagnostics tool (cargo check / tsc / go build)
    /// Hard cap for a per-call bash timeout override (the `timeout` arg on the
    /// bash tool). A model can request more time for a slow build/test, but it
    /// can't escalate past this ceiling (default 600s) without changing config.
    pub max_bash_timeout_secs: u64,
    /// Allowlist of host glob patterns the `fetch` tool may contact (e.g.
    /// `["*.rust-lang.org", "docs.rs", "crates.io"]`). Empty = allow any http(s)
    /// host (the tool is still useful out of the box, and works under
    /// `--no-network` where bash curl is dead — PROVIDED you populate this
    /// allowlist, since `--no-network` + empty allowlist denies fetch to avoid a
    /// surprise bypass of the egress block). Populate it to restrict egress.
    pub fetch_allowlist: Vec<String>,
    /// Wall-clock timeout for the `fetch` tool (default 20s).
    pub fetch_timeout_secs: u64,
    /// Max response body the `fetch` tool returns (default 256 KiB).
    pub fetch_max_bytes: usize,
    pub bash_deny: Vec<String>,
    pub max_read_bytes: u64,
    pub max_read_lines: usize,
    pub context_compact_at: f32, // fraction of context_window that triggers compaction
    pub context_digest_at: f32, // fraction of context_window that triggers stale-tool-result digesting (sub-threshold reclaim; 0 disables)
    pub auto_compact: bool, // automatically compact when context approaches the limit (threshold + idle); manual /compact always works regardless
    /// Opt-in JSONL debug log path. Records every tool call with its full
    /// arguments (file contents, bash commands) — which may include secrets the
    /// model writes (e.g. into a `.env`). User-owned, off by default, rotates at
    /// 64 MiB. Enable only when debugging.
    pub debug_log: Option<PathBuf>,
    /// Full-verbosity debug mode (`--debug` / `CATALYST_CODE_DEBUG=1`). When
    /// true, the debug log also records provider HTTP request/response bodies,
    /// full tool args+outputs, inbound commands, and a mirror of protocol
    /// events. Implies a default `debug_log` path when none is set.
    pub debug_verbose: bool,
    /// Append-only security audit sidecar (tool decisions, args hashes).
    pub audit_log: bool,
    pub session_file: Option<PathBuf>,
    pub default_model: Option<String>,
    // --- production knobs (items 3,4,7) ---
    pub sandbox: Sandbox, // Microsandbox microVM (or none)
    pub no_network: bool, // legacy --no-network: maps to sandbox_network_mode=None
    /// OCI image for the Microsandbox guest. Default is a CatCode-maintained
    /// polyglot developer image published via GHCR.
    pub sandbox_image: String,
    /// vCPUs for the guest (1..=16).
    pub sandbox_cpus: u8,
    /// Guest memory in MiB (256..=16384).
    pub sandbox_memory_mb: u32,
    /// Writable overlay disk size in MiB for the guest rootfs (256..=65536).
    pub sandbox_disk_mb: u32,
    /// Idle timeout before an unused sandbox is stopped (seconds). Reused for
    /// the session otherwise so build caches persist.
    pub sandbox_idle_timeout_secs: u64,
    /// Network egress policy for the guest.
    pub sandbox_network_mode: SandboxNetworkMode,
    /// Allowlist of domains/CIDRs for `allowlist` mode (and as an explicit
    /// permit set in `restricted` mode).
    pub sandbox_network_allowlist: Vec<String>,
    /// Whether to permit guest access to private network ranges (RFC1918/loopback).
    pub sandbox_allow_private_networks: bool,
    /// Extra host environment variables to pass into the guest (in addition to
    /// the minimal default guest env). Secrets are denied regardless.
    pub sandbox_env_allowlist: Vec<String>,
    pub idle_timeout_secs: u64,               // per-chunk SSE idle timeout
    pub max_session_tokens: u64,              // hard session token budget (0 = unlimited)
    pub summarize_on_compact: bool,           // use a model call to summarize dropped turns
    pub compact_instructions: Option<String>, // optional guidance woven into the summarize prompt (e.g. "Focus on code samples and API usage"); /compact <instructions> overrides per-call
    pub rolling_state: bool, // inject a transient tail work-state summary (KV-cache-aware)
    /// Auto-reflect: on a non-trivial turn (≥ `auto_reflect_min_tool_calls` tool
    /// calls), inject a reflection continuation BEFORE the model writes its
    /// completion summary so durable facts get persisted (memory) and recurring
    /// patterns get written as skills — without relying on the model remembering
    /// to reflect. The model then writes its summary as the final message. The
    /// deterministic seam SELF_LEARNING.md §11 deferred. Skips reflect/index
    /// turns and trivial turns. Default on.
    pub auto_reflect: bool,
    /// Minimum non-trivial tool-call count for auto-reflect to fire. 1 = any
    /// real work. 0 is treated as 1 (a no-work turn should never reflect).
    pub auto_reflect_min_tool_calls: u32,
    pub allow_vision: bool, // accept image_url content in send
    // --- permission rules (item 1) ---
    pub allow_rules: Vec<PermissionRule>,
    pub deny_rules: Vec<PermissionRule>,
    pub ask_rules: Vec<PermissionRule>,
    // --- plugin system (centerpiece) ---
    pub plugin_dir: PathBuf,           // directory scanned for plugins
    pub plugins_disabled: Vec<String>, // plugin names that are explicitly disabled
    pub trust_project_plugins: bool, // Permit repo-shipped plugins under <workspace>/.catalyst-code/plugins. Default false; set only through env/CLI so a repository cannot self-authorize. Plugins installed explicitly through /plugin-install carry a user-installed marker and remain usable without this blanket trust flag.
    // --- regex denylist upgrade (quick win) ---
    pub bash_deny_regex: Vec<String>, // regex patterns that block bash commands
    pub bash_deny_regex_compiled: Vec<regex::Regex>, // pre-compiled at startup
    // --- optional second-model advisor ---
    pub advisor: AdvisorConfig,
    pub subagents: SubagentConfig,
    /// Task-aware model routing by agent role (scout→fast, worker→strong, …).
    pub routing: RoutingConfig,
    // --- custom providers (openai/anthropic endpoints) ---
    /// Named, configurable model providers. Empty = legacy single-endpoint mode
    /// (uses `base_url` + the runtime key set via `set_key`).
    pub providers: Vec<ProviderConfig>,
    /// Name of the active provider. None = use the first configured provider, or
    /// the legacy default when none are configured.
    pub active_provider: Option<String>,
    /// Per-provider API keys persisted by the TUI (settings.json `provider_keys`
    /// + the legacy `api_key` under "default"). Seeded into `State::api_keys` at
    ///   startup so they override config/env keys (runtime keys win in provider
    ///   resolution) and survive restarts — this is what makes persisted keys sticky.
    pub persisted_keys: std::collections::HashMap<String, String>,
    /// Search-tool API keys (Exa / Tavily) set via `/search-key` and persisted
    /// to config.json `search_keys`. Searched by `web_search` before the
    /// `EXA_API_KEY` / `TAVILY_API_KEY` env vars (so slash-command keys win).
    pub search_keys: std::collections::HashMap<String, String>,
    /// MCP servers from user-owned config only; project settings cannot define transports.
    pub mcp_servers: Vec<crate::mcp::McpConfig>,
}

#[derive(Clone, Debug)]
pub struct AdvisorConfig {
    pub enabled: bool,
    pub subagents: bool,
    pub model: Option<String>,
    pub subagent_model: Option<String>,
    pub nudge: bool,
    /// Optional named watchdog roster. Loaded from WATCHDOG.yml files.
    pub watchdog: Vec<WatchdogAdvisor>,
    /// Shared WATCHDOG.yml instructions.
    pub watchdog_instructions: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct WatchdogAdvisor {
    pub name: String,
    pub enabled: bool,
    pub model: Option<String>,
    /// Read-only evidence tools requested by this specialist.
    pub tools: Vec<String>,
    /// Optional path triggers; empty means always eligible.
    pub triggers: Vec<String>,
    pub instructions: Option<String>,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            subagents: false,
            model: None,
            subagent_model: None,
            nudge: false,
            watchdog: Vec::new(),
            watchdog_instructions: None,
        }
    }
}

/// Intercom bridge mode: controls whether subagents get a coordination channel
/// back to the orchestrator and to each other.
#[derive(Clone, Debug, PartialEq)]
pub enum IntercomBridgeMode {
    Off,      // no intercom tools injected into subagents
    ForkOnly, // only for forked-context runs
    Always,   // always inject (default)
}

impl IntercomBridgeMode {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "off" | "none" => IntercomBridgeMode::Off,
            "fork-only" | "fork_only" | "fork" => IntercomBridgeMode::ForkOnly,
            _ => IntercomBridgeMode::Always,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            IntercomBridgeMode::Off => "off",
            IntercomBridgeMode::ForkOnly => "fork-only",
            IntercomBridgeMode::Always => "always",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentConfig {
    /// Max nesting depth for subagent delegation (main → sub → sub-sub).
    /// 0 blocks all subagents; default 2.
    pub max_depth: u32,
    /// Whether subagents receive intercom coordination tools + instructions.
    pub intercom_bridge_mode: IntercomBridgeMode,
    /// Max tasks in a top-level parallel run.
    pub parallel_max_tasks: u32,
    /// Default concurrency for parallel runs.
    pub parallel_concurrency: u32,
    /// Top-level calls use background execution when async is not explicitly set.
    pub async_by_default: bool,
    /// Hide builtin agents from discovery.
    pub disable_builtins: bool,
    /// Per-builtin agent overrides keyed by agent name.
    pub agent_overrides: std::collections::HashMap<String, AgentOverride>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentOverride {
    pub model: Option<String>,
    pub fallback_models: Vec<String>,
    pub thinking: Option<String>,
    pub disabled: bool,
}

/// Task-aware model routing preferences by agent role.
/// Explicit agent.model / override / goal role_models still win.
#[derive(Clone, Debug)]
pub struct RoutingConfig {
    /// Prefer cheap/fast models for these agent names (scout, researcher, …).
    pub fast_roles: Vec<String>,
    /// Prefer strong models for these (worker, reviewer, oracle, …).
    pub strong_roles: Vec<String>,
    /// Substrings that mark a model as "fast" (case-insensitive).
    pub fast_markers: Vec<String>,
    /// Substrings that mark a model as "strong".
    pub strong_markers: Vec<String>,
    pub enabled: bool,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            fast_roles: vec![
                "scout".into(),
                "researcher".into(),
                "context-builder".into(),
            ],
            strong_roles: vec![
                "worker".into(),
                "reviewer".into(),
                "oracle".into(),
                "planner".into(),
            ],
            fast_markers: vec![
                "haiku".into(),
                "flash".into(),
                "mini".into(),
                "small".into(),
                "fast".into(),
                "lite".into(),
                "nano".into(),
            ],
            strong_markers: vec![
                "opus".into(),
                "sonnet".into(),
                "pro".into(),
                "max".into(),
                "large".into(),
                "ultra".into(),
                "glm-5".into(),
            ],
            enabled: true,
        }
    }
}

impl RoutingConfig {
    pub fn preference_for(&self, agent_name: &str) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        let n = agent_name.to_ascii_lowercase();
        if self.fast_roles.iter().any(|r| r.eq_ignore_ascii_case(&n)) {
            Some("fast")
        } else if self.strong_roles.iter().any(|r| r.eq_ignore_ascii_case(&n)) {
            Some("strong")
        } else {
            None
        }
    }

    pub fn score_model(&self, model_id: &str, prefer: &str) -> i32 {
        let id = model_id.to_ascii_lowercase();
        let mut score = 0i32;
        let fast_hit = self.fast_markers.iter().any(|m| id.contains(m));
        let strong_hit = self.strong_markers.iter().any(|m| id.contains(m));
        match prefer {
            "fast" => {
                if fast_hit {
                    score += 10;
                }
                if strong_hit {
                    score -= 5;
                }
            }
            "strong" => {
                if strong_hit {
                    score += 10;
                }
                if fast_hit {
                    score -= 5;
                }
            }
            _ => {}
        }
        score
    }
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_depth: 2,
            intercom_bridge_mode: IntercomBridgeMode::Always,
            parallel_max_tasks: 64,
            parallel_concurrency: 4,
            async_by_default: false,
            disable_builtins: false,
            agent_overrides: std::collections::HashMap::new(),
        }
    }
}

/// A model provider: a named OpenAI- or Anthropic-compatible endpoint with
/// its own base URL, auth, and wire protocol. Defined in config (JSON/env);
/// switched at runtime via the `set_provider` command.
///
/// The harness keeps the *internal* conversation in OpenAI chat-completions
/// shape (role:"tool", assistant `tool_calls`, ...) because every other layer
/// (compaction, sanitization, subagents, session persistence) understands
/// that shape. The provider abstraction only translates at the HTTP boundary:
/// `kind` decides whether requests/responses are OpenAI-shaped or translated
/// to/from the Anthropic Messages API. This means adding a provider never
/// touches the rest of the harness.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum ProviderKind {
    #[default]
    OpenAI,
    Anthropic,
}

impl ProviderKind {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "anthropic" | "claude" => ProviderKind::Anthropic,
            _ => ProviderKind::OpenAI,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderKind::OpenAI => "openai",
            ProviderKind::Anthropic => "anthropic",
        }
    }
    pub fn is_openai(&self) -> bool {
        matches!(self, ProviderKind::OpenAI)
    }
    pub fn is_anthropic(&self) -> bool {
        matches!(self, ProviderKind::Anthropic)
    }
}
impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A per-model capability override: refines a single discovered model's
/// context window, max output tokens, reasoning flag, and/or advertised
/// thinking levels. Applied AFTER discovery + models.dev enrichment + the
/// per-provider `context_window` override, so an explicit per-model value wins
/// over everything else. Every field is optional — only the ones present are
/// applied; the rest fall through to the discovered/curated/default caps.
/// This lets a user hand-tune a model the endpoint under-reports (e.g. a local
/// server listing bare ids, or a gateway whose `/v1/models` omits caps) without
/// a code change, while leaving other models on the 200k/8k flat defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelOverride {
    pub id: String,
    /// Force this model's context window (tokens).
    pub context_window: Option<u32>,
    /// Force this model's max output tokens.
    pub max_tokens: Option<u32>,
    /// Force whether the model supports extended thinking/reasoning.
    pub reasoning: Option<bool>,
    /// Force the advertised reasoning effort levels (e.g. ["low","medium","high"]).
    /// An empty vec clears the levels (model declares none); `None` leaves them.
    pub thinking_levels: Option<Vec<String>>,
    /// Force accepted input modalities (e.g. ["text", "image"]).
    pub input: Option<Vec<String>>,
    /// Force emitted output modalities (e.g. ["text"]).
    pub output: Option<Vec<String>>,
    /// Force tool/function calling support.
    pub tool_call: Option<bool>,
    /// Force structured-output support.
    pub structured_output: Option<bool>,
}

/// A configured provider as it appears in the config file/env (no resolved
/// runtime key). `api_key` is a literal (user-owned config only, never a
/// project-local file); `api_key_env` names an env var to read instead.
#[derive(Clone, Debug, Default)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    /// Literal API key stored in the (user-owned, 0600) config file. Optional.
    pub api_key: Option<String>,
    /// Name of an env var holding the API key (resolved at request time).
    pub api_key_env: Option<String>,
    /// Extra HTTP headers appended to every request (e.g. `HTTP-Referer`).
    pub headers: Vec<(String, String)>,
    /// Optional per-provider context-window override (tokens). When set, every
    /// model discovered from this provider is forced to this window — for
    /// local servers (e.g. LM Studio) whose `/v1/models` returns bare ids
    /// without a context field, so the harness doesn't oversend past the
    /// model's actual loaded context. `None` = use the discovered/curated cap.
    pub context_window: Option<u32>,
    /// Optional per-model capability overrides. Applied to matching model ids
    /// after discovery + models.dev + the per-provider `context_window`. Lets
    /// the user hand-tune reasoning levels / context / output for individual
    /// models the endpoint under-reports. Empty = no per-model refinement.
    pub models_override: Vec<ModelOverride>,
    /// Optional custom models-list endpoint path (e.g. `/v1/models`,
    /// `/api/models`). When set, discovery hits `{base_url}{models_endpoint}`
    /// directly instead of the hardcoded `/models/info` (Umans) → `/models`
    /// (OpenAI) fallback chain. Lets a non-standard OpenAI-compatible endpoint
    /// work without a code branch. `None` = use the default discovery path.
    pub models_endpoint: Option<String>,
}

/// A provider fully resolved for an API call: kind, base URL, the effective API
/// key (runtime override -> config literal -> config env var -> global env),
/// and extra headers. This is what `stream_turn` / `discover_models` /
/// `summarize` consume — it carries everything provider-specific so those
/// functions stop depending on `cfg.base_url` + a bare `api_key` string.
#[derive(Clone, Debug)]
pub struct ResolvedProvider {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
    /// When true, Anthropic streaming/discovery uses `Authorization: Bearer`
    /// instead of `x-api-key` (plugin subscription OAuth). Set by
    /// `oauth::enrich_oauth` when a plugin token is injected.
    pub oauth: bool,
    /// Per-provider context-window override carried from `ProviderConfig` so
    /// `discover_models` can force it onto every discovered model. `None` =
    /// use the discovered/curated context.
    pub context_window: Option<u32>,
    /// Per-model capability overrides carried from `ProviderConfig` so
    /// `discover_models` can refine individual models after discovery. Applied
    /// in `apply_models_override` (after `apply_context_window_override`).
    pub models_override: Vec<ModelOverride>,
    /// Custom models-list endpoint path carried from `ProviderConfig` so
    /// discovery can hit a non-standard endpoint without a code branch.
    pub models_endpoint: Option<String>,
}

impl ResolvedProvider {
    /// The legacy/default provider when none are configured: OpenAI-shaped,
    /// `cfg.base_url` for the URL, key resolved only from `runtime_keys["default"]`
    /// (set via explicit `/login` / `set_key`). Does **not** scan `UMANS_API_KEY`
    /// or other env vars — a fresh install stays signed out until the user
    /// provides a key or completes OAuth.
    pub fn legacy_default(
        cfg: &Config,
        runtime_keys: &std::collections::HashMap<String, String>,
    ) -> Self {
        let api_key = runtime_keys
            .get("default")
            .cloned()
            .filter(|s| !s.is_empty());
        ResolvedProvider {
            name: "default".to_string(),
            kind: ProviderKind::OpenAI,
            base_url: cfg.base_url.clone(),
            api_key,
            headers: Vec::new(),
            oauth: false,
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }
}

/// A built-in first-party provider template: a known endpoint + the standard
/// API-key env var for that vendor, so a user can add the provider with a
/// single action (`add_provider`) instead of hand-editing JSON. Presets cover
/// the major vendors. The harness always keeps the conversation in OpenAI
/// chat-completions shape internally; a preset's `kind` only decides the wire
/// translation at the HTTP boundary (Gemini exposes an OpenAI-compatible
/// endpoint, so it maps to `OpenAI`).
#[derive(Clone, Debug)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    /// Primary env var holding the API key (e.g. `OPENAI_API_KEY`).
    pub api_key_env: &'static str,
    /// Alternate env vars checked in order if the primary is unset
    /// (e.g. Gemini accepts `GOOGLE_API_KEY` too).
    pub alt_envs: &'static [&'static str],
    pub description: &'static str,
}

/// The first-party provider presets. Order is the order shown in pickers.
/// Official provider bundles under `core/providers/` carry provider-specific
/// metadata and documentation so removable vendors stay isolated. Umans is
/// listed first as the default/original provider.
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "umans",
        label: "Umans (GLM-5.2)",
        kind: ProviderKind::OpenAI,
        // The default Umans endpoint. is_umans() matches `umans.ai` as a parent
        // domain, so the GLM-specific wire logic (reasoning_effort,
        // /models/info discovery) still applies to this preset's turns.
        base_url: "https://api.code.umans.ai/v1",
        api_key_env: "UMANS_API_KEY",
        alt_envs: &[],
        description: "Umans — GLM-5.2, the default provider. Paste your API key via /login (https://app.umans.ai/billing → API Keys).",
    },
    ProviderPreset {
        id: "opencode-go",
        label: "OpenCode Go",
        kind: ProviderKind::OpenAI,
        // OpenCode Go is one subscription/key that serves some models via an
        // OpenAI-compatible `/v1/chat/completions` endpoint and others via an
        // Anthropic `/v1/messages` endpoint. preset_provider_configs()
        // expands this preset into TWO provider configs (opencode-go +
        // opencode-go-anthropic) sharing this base URL + key, so each model
        // routes to its correct wire protocol. See provider::is_opencode_go.
        base_url: "https://opencode.ai/zen/go/v1",
        api_key_env: "OPENCODE_GO_API_KEY",
        alt_envs: &[],
        description: "OpenCode Go — low-cost subscription for popular open coding models (GLM, Kimi, DeepSeek, MiMo, MiniMax, Qwen). One API key; models route to the OpenAI-compatible or Anthropic endpoint automatically. Uses your OPENCODE_GO_API_KEY.",
    },
    ProviderPreset {
        id: "openrouter",
        label: "OpenRouter",
        kind: ProviderKind::OpenAI,
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        alt_envs: &[],
        description: "OpenRouter multi-model gateway. Uses OPENROUTER_API_KEY (https://openrouter.ai/settings/keys).",
    },
    ProviderPreset {
        id: "deepseek",
        label: "DeepSeek",
        kind: ProviderKind::OpenAI,
        // DeepSeek's official OpenAI-compatible base URL intentionally does
        // not include `/v1`; the API appends `/chat/completions` and `/models`
        // directly to this base.
        base_url: "https://api.deepseek.com",
        api_key_env: "DEEPSEEK_API_KEY",
        alt_envs: &[],
        description: "DeepSeek API — V4 Pro and V4 Flash with automatic live model discovery. Uses DEEPSEEK_API_KEY (https://platform.deepseek.com/api_keys).",
    },
];

/// Look up a first-party preset by id.
pub fn find_preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDER_PRESETS.iter().find(|p| p.id == id)
}

impl ProviderPreset {
    /// Resolve an API key for this preset from its env vars (primary first,
    /// then alternates). None when none are set. Empty `api_key_env` means
    /// OAuth-only (e.g. xAI SuperGrok) — always returns None.
    ///
    /// Not used for auto-login (auth is explicit via `/login` paste or OAuth).
    /// Kept for diagnostics and for callers that intentionally opt into env lookup.
    #[allow(dead_code)]
    pub fn env_key(&self) -> Option<String> {
        if self.api_key_env.is_empty() {
            return None;
        }
        std::env::var(self.api_key_env)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.alt_envs
                    .iter()
                    .find_map(|e| std::env::var(e).ok().filter(|s| !s.is_empty()))
            })
    }

    /// The env var name that actually held a key, or the primary env var when
    /// none are set (so a future `export` Just Works without re-adding).
    pub fn resolved_env(&self) -> &'static str {
        if std::env::var(self.api_key_env)
            .ok()
            .filter(|s| !s.is_empty())
            .is_some()
        {
            return self.api_key_env;
        }
        for e in self.alt_envs {
            if std::env::var(e).ok().filter(|s| !s.is_empty()).is_some() {
                return e;
            }
        }
        self.api_key_env
    }

    /// Build a `ProviderConfig` from this preset. When `api_key` is given it is
    /// stored as a literal (user entered it); otherwise the key is read from the
    /// preset's env var and only the env-var NAME is persisted (the secret stays
    /// in the environment, never copied into the config file).
    pub fn to_provider_config(&self, api_key: Option<String>) -> ProviderConfig {
        let (api_key, api_key_env) = match api_key {
            Some(k) => (Some(k), None),
            None => (None, Some(self.resolved_env().to_string())),
        };
        ProviderConfig {
            name: self.id.to_string(),
            kind: self.kind.clone(),
            base_url: self.base_url.to_string(),
            api_key,
            api_key_env,
            headers: Vec::new(),
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        }
    }
}

/// The provider config(s) to create when logging in to a preset. Most presets
/// map to a single config; OpenCode Go maps to TWO — one OpenAI-kind (for its
/// `/v1/chat/completions` models: GLM, Kimi, DeepSeek, MiMo) and one
/// Anthropic-kind (for its `/v1/messages` models: MiniMax, Qwen) — because
/// OpenCode Go serves models over both wire protocols under one API key, and
/// the harness's per-provider `kind` decides the wire translation at the HTTP
/// boundary. Both configs share the preset's base URL + key. The first config
/// is the "primary" (used as the active provider + preset identity).
pub fn preset_provider_configs(p: &ProviderPreset, api_key: Option<String>) -> Vec<ProviderConfig> {
    if p.id == "opencode-go" {
        let (api_key_lit, api_key_env) = match api_key {
            Some(k) => (Some(k), None),
            None => (None, Some(p.resolved_env().to_string())),
        };
        let make = |name: &str, kind: ProviderKind| ProviderConfig {
            name: name.to_string(),
            kind,
            base_url: p.base_url.to_string(),
            api_key: api_key_lit.clone(),
            api_key_env: api_key_env.clone(),
            headers: Vec::new(),
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        };
        vec![
            make("opencode-go", ProviderKind::OpenAI),
            make("opencode-go-anthropic", ProviderKind::Anthropic),
        ]
    } else {
        vec![p.to_provider_config(api_key)]
    }
}

/// Serialize a `ProviderConfig` back to JSON for persistence. Only writes
/// non-default fields so the file stays readable.
pub fn provider_to_json(p: &ProviderConfig) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("name".into(), json!(p.name));
    o.insert("kind".into(), json!(p.kind.as_str()));
    o.insert("base_url".into(), json!(p.base_url));
    if let Some(k) = &p.api_key {
        o.insert("api_key".into(), json!(k));
    }
    if let Some(e) = &p.api_key_env {
        o.insert("api_key_env".into(), json!(e));
    }
    if !p.headers.is_empty() {
        let h: serde_json::Map<String, Value> = p
            .headers
            .iter()
            .cloned()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        o.insert("headers".into(), Value::Object(h));
    }
    if let Some(ctx) = p.context_window {
        o.insert("context_window".into(), json!(ctx));
    }
    if !p.models_override.is_empty() {
        let arr: Vec<Value> = p
            .models_override
            .iter()
            .map(model_override_to_json)
            .collect();
        o.insert("models_override".into(), json!(arr));
    }
    Value::Object(o)
}

/// Serialize a `ModelOverride` back to JSON for persistence. Only writes
/// non-default fields so the file stays readable.
fn model_override_to_json(m: &ModelOverride) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("id".into(), json!(m.id));
    if let Some(ctx) = m.context_window {
        o.insert("context_window".into(), json!(ctx));
    }
    if let Some(max) = m.max_tokens {
        o.insert("max_tokens".into(), json!(max));
    }
    if let Some(r) = m.reasoning {
        o.insert("reasoning".into(), json!(r));
    }
    if let Some(levels) = &m.thinking_levels {
        o.insert("thinking_levels".into(), json!(levels));
    }
    if let Some(input) = &m.input {
        o.insert("input".into(), json!(input));
    }
    if let Some(output) = &m.output {
        o.insert("output".into(), json!(output));
    }
    if let Some(v) = m.tool_call {
        o.insert("tool_call".into(), json!(v));
    }
    if let Some(v) = m.structured_output {
        o.insert("structured_output".into(), json!(v));
    }
    Value::Object(o)
}

/// Path of the core-owned config file (`~/.config/catalyst-code/config.json`),
/// where first-party providers added at runtime are persisted. The TUI does
/// NOT write this file (it owns `settings.json`), so there is no clobber.
pub fn user_config_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".config/catalyst-code/config.json"))
}

/// Persist `providers` (+ optional active provider) into the core-owned config
/// file, merging with any existing JSON so other keys are preserved. Atomic
/// (temp + rename) with 0600 perms. Best-effort: returns an io error on failure.
pub fn save_providers_config(
    providers: &[ProviderConfig],
    active: Option<&str>,
) -> std::io::Result<()> {
    let path = user_config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    // Cross-process lock: this is a read-modify-write (merge providers into
    // existing config.json). Two processes logging in different providers
    // concurrently would otherwise race and one provider's entry would be lost.
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))?;
    // Read existing config.json (if any) and merge so other keys survive.
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if root.as_object().is_none() {
        root = json!({});
    }
    let arr: Vec<Value> = providers.iter().map(provider_to_json).collect();
    root["providers"] = json!(arr);
    if let Some(a) = active {
        root["activeProvider"] = json!(a);
    }
    let data = serde_json::to_string_pretty(&root).unwrap_or_default();
    // Unique-temp atomic write + 0600 (fsutil): no shared-temp collision.
    crate::fsutil::atomic_write_secure(&path, data.as_bytes())?;
    Ok(())
}

/// Persist the search-tool API keys (Exa / Tavily) to config.json's
/// `search_keys` object. Same atomic read-merge-write + cross-process lock
/// as `save_providers_config` so a concurrent provider login can't clobber it.
/// An empty value removes the entry (so `/search-key exa --clear` works).
pub fn save_search_keys(keys: &std::collections::HashMap<String, String>) -> std::io::Result<()> {
    let path = user_config_path()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))?;
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if root.as_object().is_none() {
        root = json!({});
    }
    if keys.is_empty() {
        if let Some(obj) = root.as_object_mut() {
            obj.remove("search_keys");
        }
    } else {
        let mut map = serde_json::Map::new();
        for (k, v) in keys {
            if !v.is_empty() {
                map.insert(k.clone(), json!(v));
            }
        }
        root["search_keys"] = Value::Object(map);
    }
    let data = serde_json::to_string_pretty(&root).unwrap_or_default();
    crate::fsutil::atomic_write_secure(&path, data.as_bytes())?;
    Ok(())
}

/// Previously auto-added every preset whose env API key was already present.
/// That silently signed users in on first launch. Auth is now explicit only:
/// paste an API key via `/login` or complete a **plugin** OAuth flow. Kept as a
/// no-op so call sites stay stable.
pub fn auto_login_env_presets(_cfg: &mut Config) -> Vec<String> {
    Vec::new()
}

impl Config {
    pub fn find_provider(&self, name: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// Names of configured providers in declaration order.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.iter().map(|p| p.name.clone()).collect()
    }

    /// Resolve the active provider into a `ResolvedProvider`, combining the
    /// config definition with per-provider runtime keys (set via `set_key`).
    ///
    /// Resolution order for the active provider:
    ///   1. `active_provider` (if it names a configured provider)
    ///   2. the first configured provider (if any)
    ///   3. the legacy default (OpenAI, `cfg.base_url`) when none are configured
    ///
    /// API key for the resolved provider:
    ///   runtime_keys[name] -> provider.api_key -> provider.api_key_env (env).
    /// Empty values are dropped. The legacy default (no providers configured)
    /// only uses runtime_keys["default"] — it does not scan env vars.
    pub fn resolve_provider(
        &self,
        runtime_keys: &std::collections::HashMap<String, String>,
    ) -> ResolvedProvider {
        self.resolve_provider_with(runtime_keys, None)
    }

    /// Like `resolve_provider` but with an optional runtime override of the
    /// active provider name (set via `set_provider`). The override wins over
    /// the config's `active_provider`; an unknown override falls back to the
    /// first configured provider, then the legacy default.
    pub fn resolve_provider_with(
        &self,
        runtime_keys: &std::collections::HashMap<String, String>,
        active_override: Option<&str>,
    ) -> ResolvedProvider {
        let pick = match active_override.or(self.active_provider.as_deref()) {
            Some(name) => self
                .find_provider(name)
                .cloned()
                .or_else(|| self.providers.first().cloned()),
            None => self.providers.first().cloned(),
        };
        let Some(p) = pick else {
            return ResolvedProvider::legacy_default(self, runtime_keys);
        };
        let api_key = runtime_keys
            .get(&p.name)
            .cloned()
            .or_else(|| p.api_key.clone())
            .or_else(|| p.api_key_env.as_ref().and_then(|v| std::env::var(v).ok()))
            .filter(|s| !s.is_empty());
        ResolvedProvider {
            name: p.name.clone(),
            kind: p.kind,
            base_url: p.base_url.clone(),
            api_key,
            headers: p.headers.clone(),
            oauth: false,
            context_window: p.context_window,
            models_override: p.models_override.clone(),
            models_endpoint: p.models_endpoint.clone(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "https://api.code.umans.ai/v1".into(),
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            approval: Approval::Destructive,
            bash_timeout_secs: 30,
            diag_timeout_secs: 120, // builds/checks can run longer than bash; bounded so a hung checker can't wedge the turn
            max_bash_timeout_secs: 600, // a model may ask up to 10 min for one command, but no more
            fetch_allowlist: Vec::new(), // empty = allow any http(s) host; populate to restrict
            fetch_timeout_secs: 20,
            fetch_max_bytes: 262_144, // 256 KiB — enough for a doc page, bounded so a giant response can't OOM
            bash_deny: vec![
                // ponytail: minimal denylist of obviously catastrophic commands.
                // Not a security boundary (use a sandbox for that); a tripwire.
                "rm -rf /".into(),
                "rm -rf ~".into(),
                "mkfs".into(),
                "dd if=/dev/zero of=/dev/sd".into(),
                ":(){:|:&};:".into(),
            ],
            max_read_bytes: 5_242_880, // 5 MiB (was 1 MiB; real files exceed 1MB)
            max_read_lines: 10_000,    // was 2000; pagination covers the rest
            context_compact_at: 0.90,
            context_digest_at: 0.70,
            debug_log: None,
            debug_verbose: false,
            audit_log: false,
            session_file: None,
            default_model: None,
            sandbox: Sandbox::None,
            no_network: false,
            sandbox_image: DEFAULT_SANDBOX_IMAGE.to_string(),
            sandbox_cpus: DEFAULT_SANDBOX_CPUS,
            sandbox_memory_mb: DEFAULT_SANDBOX_MEMORY_MB,
            sandbox_disk_mb: DEFAULT_SANDBOX_DISK_MB,
            sandbox_idle_timeout_secs: DEFAULT_SANDBOX_IDLE_TIMEOUT_SECS,
            sandbox_network_mode: SandboxNetworkMode::Restricted,
            sandbox_network_allowlist: Vec::new(),
            sandbox_allow_private_networks: false,
            sandbox_env_allowlist: Vec::new(),
            idle_timeout_secs: 120, // some reasoning models think >60s before first token
            max_session_tokens: 0,
            summarize_on_compact: true,
            compact_instructions: None,
            auto_compact: true,
            rolling_state: true,
            auto_reflect: true,
            auto_reflect_min_tool_calls: 1,
            allow_vision: true,
            allow_rules: Vec::new(),
            deny_rules: Vec::new(),
            ask_rules: Vec::new(),
            plugin_dir: PathBuf::from(".catalyst-code/plugins"),
            plugins_disabled: Vec::new(),
            trust_project_plugins: false, // default: do not execute repo-shipped plugin scripts
            bash_deny_regex: Vec::new(),
            bash_deny_regex_compiled: Vec::new(),
            advisor: AdvisorConfig::default(),
            subagents: SubagentConfig::default(),
            routing: RoutingConfig::default(),
            providers: Vec::new(),
            active_provider: None,
            persisted_keys: std::collections::HashMap::new(),
            search_keys: std::collections::HashMap::new(),
            mcp_servers: Vec::new(),
        }
    }
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default Microsandbox guest image — a CatCode-maintained polyglot developer
/// image (bash, coreutils, git, curl, jq, ripgrep, tar, build-essential, plus
/// Rust/Node/Python/Go toolchains) published via GHCR. Pinned by tag; releases
/// record the resolved digest. Override with `sandbox_image`.
pub const DEFAULT_SANDBOX_IMAGE: &str = "ghcr.io/catalystctl/catcode-sandbox:0.1";
pub const DEFAULT_SANDBOX_CPUS: u8 = 2;
pub const DEFAULT_SANDBOX_MEMORY_MB: u32 = 2048;
pub const DEFAULT_SANDBOX_DISK_MB: u32 = 8192;
pub const DEFAULT_SANDBOX_IDLE_TIMEOUT_SECS: u64 = 900;
/// Hard caps for sandbox resource validation.
pub const SANDBOX_MIN_CPUS: u8 = 1;
pub const SANDBOX_MAX_CPUS: u8 = 16;
pub const SANDBOX_MIN_MEMORY_MB: u32 = 256;
pub const SANDBOX_MAX_MEMORY_MB: u32 = 16384;
pub const SANDBOX_MIN_DISK_MB: u32 = 256;
pub const SANDBOX_MAX_DISK_MB: u32 = 65536;
pub const HELP: &str = "\
catalyst-code-core — OpenAI-compatible coding agent core (native Umans)

USAGE:
  core [OPTIONS]

OPTIONS:
      --workspace <DIR>         Workspace root (constrains all file/bash ops) [env: CATALYST_CODE_WORKSPACE]
      --base-url <URL>          OpenAI-compatible base URL [env: UMANS_BASE_URL]
      --approval <MODE>         never | destructive | always  [env: CATALYST_CODE_APPROVAL]
      --bash-timeout <SECS>     Per-command bash timeout in seconds [env: CATALYST_CODE_BASH_TIMEOUT]
      --max-bash-timeout <SECS>  Ceiling for the bash tool's per-call `timeout` override [env: CATALYST_CODE_MAX_BASH_TIMEOUT]
      --fetch-timeout <SECS>    Wall-clock timeout for the `fetch` tool [env: CATALYST_CODE_FETCH_TIMEOUT]
      --diag-timeout <SECS>     Diagnostics tool (cargo check/tsc/go build) timeout in seconds [env: CATALYST_CODE_DIAG_TIMEOUT]
      --sandbox <MODE>          none | microsandbox  (run agent workloads in a Microsandbox microVM) [env: CATALYST_CODE_SANDBOX]
      --sandbox-image <REF>      OCI image for the sandbox guest [env: CATALYST_CODE_SANDBOX_IMAGE]
      --sandbox-cpus <N>         Guest vCPUs (1..16) [env: CATALYST_CODE_SANDBOX_CPUS]
      --sandbox-memory-mb <N>    Guest memory in MiB [env: CATALYST_CODE_SANDBOX_MEMORY_MB]
      --sandbox-disk-mb <N>      Guest writable overlay in MiB [env: CATALYST_CODE_SANDBOX_DISK_MB]
      --sandbox-network <MODE>   none | restricted | allowlist  [env: CATALYST_CODE_SANDBOX_NETWORK]
      --no-network               Backward-compat: deny all guest network (sets sandbox-network=none) [env: CATALYST_CODE_NO_NETWORK=1]
      --trust-project-plugins  Allow repo-shipped .catalyst-code/plugins scripts to load [env: CATALYST_CODE_TRUST_PROJECT_PLUGINS=1]
      --idle-timeout <SECS>    SSE idle timeout in seconds [env: CATALYST_CODE_IDLE_TIMEOUT]
      --max-session-tokens <N> Hard session token budget (0=unlimited) [env: CATALYST_CODE_MAX_SESSION_TOKENS]
      --debug-log <FILE>        Structured JSONL debug log [env: CATALYST_CODE_DEBUG_LOG]
      --debug                   Full-verbosity debug mode (HTTP bodies, tool args, events) [env: CATALYST_CODE_DEBUG=1]
      --session <FILE>          Append-only JSONL session file (resume on restart) [env: CATALYST_CODE_SESSION]
      --model <ID>              Default model id
      --provider <NAME>        Active model provider (openai/anthropic endpoint; see `providers` in config) [env: UMANS_ACTIVE_PROVIDER]
      --config <FILE>           JSON config file (defaults: ./catalyst-code.json, ~/.config/catalyst-code/config.json)
  -h, --help                    Print this help
  -V, --version                 Print version

The core speaks newline-delimited JSON over stdio (commands in, events out). See README.
";

/// Parse CLI args, env vars, and config file into a Config.
///
/// Precedence (highest last): defaults → config files → env → CLI flags.
/// CLI is applied last so `--approval` from the TUI cannot be overwritten by
/// `~/.config/catalyst-code/settings.json` (or the reverse race on restart).
pub fn load() -> Config {
    let mut c = Config::default();
    let mut config_file: Option<PathBuf> = None;
    let mut help = false;
    let mut version = false;
    // CLI flags collected here and re-applied after files + env.
    let mut cli_approval: Option<Approval> = None;
    let mut cli_workspace: Option<PathBuf> = None;
    let mut cli_base_url: Option<String> = None;
    let mut cli_bash_timeout: Option<u64> = None;
    let mut cli_max_bash_timeout: Option<u64> = None;
    let mut cli_fetch_timeout: Option<u64> = None;
    let mut cli_diag_timeout: Option<u64> = None;
    let mut cli_trust_project: Option<bool> = None;
    let mut cli_debug_log: Option<PathBuf> = None;
    let mut cli_debug_verbose: Option<bool> = None;
    let mut cli_session: Option<PathBuf> = None;
    let mut cli_model: Option<String> = None;
    let mut cli_provider: Option<String> = None;
    let mut cli_sandbox: Option<Sandbox> = None;
    let mut cli_no_network: Option<bool> = None;
    let mut cli_sandbox_image: Option<String> = None;
    let mut cli_sandbox_cpus: Option<u8> = None;
    let mut cli_sandbox_memory: Option<u32> = None;
    let mut cli_sandbox_disk: Option<u32> = None;
    let mut cli_sandbox_network: Option<SandboxNetworkMode> = None;
    let mut cli_sandbox_env_allowlist: Option<Vec<String>> = None;
    let mut cli_idle_timeout: Option<u64> = None;
    let mut cli_max_session_tokens: Option<u64> = None;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        let take_val = |i: &mut usize| -> Option<String> {
            if *i + 1 < args.len() {
                *i += 1;
                Some(args[*i].clone())
            } else {
                None
            }
        };
        match a.as_str() {
            "-h" | "--help" => help = true,
            "-V" | "--version" => version = true,
            "--workspace" => {
                if let Some(v) = take_val(&mut i) {
                    cli_workspace = Some(PathBuf::from(v));
                }
            }
            "--base-url" => {
                if let Some(v) = take_val(&mut i) {
                    cli_base_url = Some(v);
                }
            }
            "--approval" => {
                if let Some(v) = take_val(&mut i) {
                    cli_approval = Some(Approval::parse(&v));
                }
            }
            "--bash-timeout" => {
                if let Some(v) = take_val(&mut i) {
                    cli_bash_timeout = v.parse().ok();
                }
            }
            "--max-bash-timeout" => {
                if let Some(v) = take_val(&mut i) {
                    cli_max_bash_timeout = v.parse().ok();
                }
            }
            "--fetch-timeout" => {
                if let Some(v) = take_val(&mut i) {
                    cli_fetch_timeout = v.parse().ok();
                }
            }
            "--diag-timeout" => {
                if let Some(v) = take_val(&mut i) {
                    cli_diag_timeout = v.parse().ok();
                }
            }
            "--trust-project-plugins" => {
                cli_trust_project = Some(true);
            }
            "--no-trust-project-plugins" => {
                cli_trust_project = Some(false);
            }
            "--debug-log" => {
                if let Some(v) = take_val(&mut i) {
                    cli_debug_log = Some(PathBuf::from(v));
                }
            }
            "--debug" => {
                cli_debug_verbose = Some(true);
            }
            "--session" => {
                if let Some(v) = take_val(&mut i) {
                    cli_session = Some(PathBuf::from(v));
                }
            }
            "--model" => {
                if let Some(v) = take_val(&mut i) {
                    cli_model = Some(v);
                }
            }
            "--provider" => {
                if let Some(v) = take_val(&mut i) {
                    cli_provider = Some(v);
                }
            }
            "--sandbox" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox = Some(parse_sandbox_setting(&v));
                }
            }
            "--no-network" => {
                cli_no_network = Some(true);
            }
            "--sandbox-image" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_image = Some(v);
                }
            }
            "--sandbox-cpus" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_cpus = v.parse().ok();
                }
            }
            "--sandbox-memory-mb" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_memory = v.parse().ok();
                }
            }
            "--sandbox-disk-mb" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_disk = v.parse().ok();
                }
            }
            "--sandbox-network" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_network = Some(SandboxNetworkMode::parse(&v));
                }
            }
            "--sandbox-env-allowlist" => {
                if let Some(v) = take_val(&mut i) {
                    cli_sandbox_env_allowlist = Some(
                        v.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    );
                }
            }
            "--idle-timeout" => {
                if let Some(v) = take_val(&mut i) {
                    cli_idle_timeout = v.parse().ok();
                }
            }
            "--max-session-tokens" => {
                if let Some(v) = take_val(&mut i) {
                    cli_max_session_tokens = v.parse().ok();
                }
            }
            "--config" => {
                if let Some(v) = take_val(&mut i) {
                    config_file = Some(PathBuf::from(v));
                }
            }
            _ => { /* ignore unknown */ }
        }
        i += 1;
    }
    if help {
        print!("{HELP}");
        std::process::exit(0);
    }
    if version {
        println!("catalyst-code-core {VERSION}");
        std::process::exit(0);
    }

    // Layer 1: config files (lowest precedence).
    let candidates: Vec<(PathBuf, bool)> = match config_file {
        Some(p) => vec![(p, false)],
        None => {
            let managed = dirs_config_path();
            let managed_dir = managed.with_file_name("catalyst-code.d");
            let home = home_dir().unwrap_or_default();
            let settings_path = home.join(".config/catalyst-code/settings.json");
            let local = PathBuf::from("settings.local.json");
            let proj = PathBuf::from("settings.json");
            let mut v = vec![(managed, false)];
            if let Ok(rd) = std::fs::read_dir(&managed_dir) {
                let mut entries: Vec<PathBuf> = rd
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
                    .collect();
                entries.sort();
                v.extend(entries.into_iter().map(|p| (p, false)));
            }
            v.push((settings_path, false));
            v.push((proj, true));
            v.push((local, true));
            v
        }
    };
    for (p, untrusted) in candidates {
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Ok(mut v) = serde_json::from_str::<Value>(&content) {
                if untrusted {
                    strip_untrusted_keys(&mut v, &p);
                }
                apply_json(&mut c, &v);
            }
        }
    }

    // Layer 2: env vars.
    if let Ok(v) = std::env::var("UMANS_BASE_URL") {
        c.base_url = v;
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_WORKSPACE") {
        c.workspace = PathBuf::from(v);
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_APPROVAL") {
        c.approval = Approval::parse(&v);
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_BASH_TIMEOUT") {
        c.bash_timeout_secs = v.parse().unwrap_or(c.bash_timeout_secs);
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_MAX_BASH_TIMEOUT") {
        if let Ok(n) = v.parse::<u64>() {
            c.max_bash_timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_FETCH_TIMEOUT") {
        if let Ok(n) = v.parse::<u64>() {
            c.fetch_timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_FETCH_MAX_BYTES") {
        if let Ok(n) = v.parse::<usize>() {
            c.fetch_max_bytes = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_FETCH_ALLOWLIST") {
        c.fetch_allowlist = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_DIAG_TIMEOUT") {
        if let Ok(n) = v.parse::<u64>() {
            c.diag_timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_TRUST_PROJECT_PLUGINS") {
        c.trust_project_plugins = v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_DEBUG_LOG") {
        c.debug_log = Some(PathBuf::from(v));
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_DEBUG") {
        let on = v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true");
        c.debug_verbose = on;
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SESSION") {
        c.session_file = Some(PathBuf::from(v));
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX") {
        c.sandbox = parse_sandbox_setting(&v);
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_NO_NETWORK") {
        let on = v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true");
        c.no_network = on;
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_IMAGE") {
        if !v.trim().is_empty() {
            c.sandbox_image = v;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_CPUS") {
        if let Ok(n) = v.parse::<u8>() {
            c.sandbox_cpus = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_MEMORY_MB") {
        if let Ok(n) = v.parse::<u32>() {
            c.sandbox_memory_mb = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_DISK_MB") {
        if let Ok(n) = v.parse::<u32>() {
            c.sandbox_disk_mb = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_NETWORK") {
        c.sandbox_network_mode = SandboxNetworkMode::parse(&v);
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_SANDBOX_ENV_ALLOWLIST") {
        c.sandbox_env_allowlist = v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_IDLE_TIMEOUT") {
        if let Ok(n) = v.parse::<u64>() {
            c.idle_timeout_secs = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_MAX_SESSION_TOKENS") {
        if let Ok(n) = v.parse::<u64>() {
            c.max_session_tokens = n;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_AUTO_REFLECT") {
        let on = v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true");
        let off = v == "0" || v.eq_ignore_ascii_case("false");
        if off {
            c.auto_reflect = false;
        } else if on {
            c.auto_reflect = true;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_AUTO_REFLECT_MIN_TOOL_CALLS") {
        if let Ok(n) = v.parse::<u32>() {
            c.auto_reflect_min_tool_calls = n.max(1);
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_AUTO_COMPACT") {
        let on = v.is_empty() || v == "1" || v.eq_ignore_ascii_case("true");
        let off = v == "0" || v.eq_ignore_ascii_case("false");
        if off {
            c.auto_compact = false;
        } else if on {
            c.auto_compact = true;
        }
    }
    if let Ok(v) = std::env::var("CATALYST_CODE_COMPACT_INSTRUCTIONS") {
        if v.trim().is_empty() {
            c.compact_instructions = None;
        } else {
            c.compact_instructions = Some(v);
        }
    }

    if let Ok(v) = std::env::var("UMANS_PROVIDERS") {
        if let Ok(arr) = serde_json::from_str::<Value>(&v) {
            if let Some(list) = arr.as_array() {
                for p in list {
                    if let Some(pc) = parse_provider(p) {
                        if !c.providers.iter().any(|x| x.name == pc.name) {
                            c.providers.push(pc);
                        }
                    }
                }
            }
        }
    }
    if let Ok(v) = std::env::var("UMANS_ACTIVE_PROVIDER") {
        c.active_provider = Some(v);
    }

    // Layer 3: CLI flags (highest precedence).
    if let Some(v) = cli_workspace {
        c.workspace = v;
    }
    if let Some(v) = cli_base_url {
        c.base_url = v;
    }
    if let Some(v) = cli_approval {
        c.approval = v;
    }
    if let Some(v) = cli_bash_timeout {
        c.bash_timeout_secs = v;
    }
    if let Some(v) = cli_max_bash_timeout {
        c.max_bash_timeout_secs = v;
    }
    if let Some(v) = cli_fetch_timeout {
        c.fetch_timeout_secs = v;
    }
    if let Some(v) = cli_diag_timeout {
        c.diag_timeout_secs = v;
    }
    if let Some(v) = cli_trust_project {
        c.trust_project_plugins = v;
    }
    if let Some(v) = cli_debug_log {
        c.debug_log = Some(v);
    }
    if let Some(v) = cli_debug_verbose {
        c.debug_verbose = v;
    }
    // Verbose mode always needs a log file. If the user only passed --debug
    // (no --debug-log / env), default to ~/.config/catalyst-code/debug.jsonl
    // so something actually gets written.
    if c.debug_verbose && c.debug_log.is_none() {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
        c.debug_log = Some(home.join(".config/catalyst-code/debug.jsonl"));
    }
    if let Some(v) = cli_session {
        c.session_file = Some(v);
    }
    if let Some(v) = cli_model {
        c.default_model = Some(v);
    }
    if let Some(v) = cli_provider {
        c.active_provider = Some(v);
    }
    if let Some(v) = cli_sandbox {
        c.sandbox = v;
    }
    if let Some(v) = cli_no_network {
        c.no_network = v;
    }
    if let Some(v) = cli_sandbox_image {
        c.sandbox_image = v;
    }
    if let Some(v) = cli_sandbox_cpus {
        c.sandbox_cpus = v;
    }
    if let Some(v) = cli_sandbox_memory {
        c.sandbox_memory_mb = v;
    }
    if let Some(v) = cli_sandbox_disk {
        c.sandbox_disk_mb = v;
    }
    if let Some(v) = cli_sandbox_network {
        c.sandbox_network_mode = v;
    }
    if let Some(v) = cli_sandbox_env_allowlist {
        c.sandbox_env_allowlist = v;
    }
    if let Some(v) = cli_idle_timeout {
        c.idle_timeout_secs = v;
    }
    if let Some(v) = cli_max_session_tokens {
        c.max_session_tokens = v;
    }

    crate::advisor::load_watchdog_configuration(&mut c);
    normalize_context_thresholds(&mut c);
    normalize_sandbox_config(&mut c);

    c.bash_deny_regex_compiled = c
        .bash_deny_regex
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();

    c
}

/// Cross-platform home directory: prefers `$HOME`, falls back to `$USERPROFILE`
/// (Windows). Returns None if neither is set.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    if let Some(h) = std::env::var_os("USERPROFILE") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    None
}

/// Cross-platform config base: `~/.config/catalyst-code` on Unix, and
/// `%USERPROFILE%\.config\catalyst-code` on Windows (kept under the same
/// relative path so settings are shared across shells / WSL).
fn dirs_config_path() -> PathBuf {
    let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/catalyst-code/config.json")
}

/// Keys a project-scoped config file (./settings.json, ./settings.local.json —
/// shipped by the repo, potentially untrusted) must NOT be allowed to set.
///
/// Loading these would let a hostile repo silently disable the approval gate /
/// sandbox / network block, widen the bash denylist / permission rules, or —
/// worst — define a provider whose `base_url` is an attacker endpoint and whose
/// `activeProvider` selection routes the user's stored OAuth subscription
/// tokens to it (the tokens resolve by provider name in `enrich_oauth`). These
/// are stripped before `apply_json`; only user-owned config (~/.config,
/// managed.d, env, CLI, explicit --config) may set them. Per-project model /
/// theme / timeout / subagent preferences remain allowed (they are not
/// security-sensitive).
fn strip_untrusted_keys(v: &mut Value, path: &std::path::Path) {
    const STRIPPED: &[&str] = &[
        "base_url",
        "approval",
        "sandbox",
        "no_network",
        "sandbox_image",
        "sandbox_network_mode",
        "sandbox_network_allowlist",
        "sandbox_allow_private_networks",
        "sandbox_cpus",
        "sandbox_memory_mb",
        "sandbox_disk_mb",
        "fetch_allowlist",
        "bash_deny",
        "bash_deny_regex",
        "permissions",
        "providers",
        "provider_keys",
        "api_key",
        "search_keys",
        "plugins",
        "mcp_servers",
    ];
    if let Some(obj) = v.as_object_mut() {
        let removed: Vec<&str> = STRIPPED
            .iter()
            .copied()
            .filter(|k| obj.contains_key(*k))
            .collect();
        if !removed.is_empty() {
            for k in &removed {
                obj.remove(*k);
            }
            eprintln!(
                "[catalyst-code] ignored security-sensitive config keys {:?} from project file {} (set them via env vars or ~/.config/catalyst-code/settings.json instead)",
                removed,
                path.display()
            );
        }
    }
}

fn apply_json(c: &mut Config, v: &Value) {
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    if let Some(x) = s("base_url") {
        c.base_url = x;
    }
    if let Some(x) = s("workspace") {
        c.workspace = PathBuf::from(x);
    }
    if let Some(x) = s("approval") {
        c.approval = Approval::parse(&x);
    }
    if let Some(b) = v.get("bash_timeout_secs").and_then(|x| x.as_u64()) {
        c.bash_timeout_secs = b;
    }
    if let Some(b) = v.get("diag_timeout_secs").and_then(|x| x.as_u64()) {
        c.diag_timeout_secs = b;
    }
    if let Some(b) = v.get("max_bash_timeout_secs").and_then(|x| x.as_u64()) {
        c.max_bash_timeout_secs = b;
    }
    if let Some(b) = v.get("fetch_timeout_secs").and_then(|x| x.as_u64()) {
        c.fetch_timeout_secs = b;
    }
    if let Some(b) = v.get("fetch_max_bytes").and_then(|x| x.as_u64()) {
        c.fetch_max_bytes = b as usize;
    }
    if let Some(arr) = v.get("fetch_allowlist").and_then(|x| x.as_array()) {
        c.fetch_allowlist = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    if let Some(x) = s("sandbox") {
        c.sandbox = parse_sandbox_setting(&x);
    }
    if let Some(b) = v.get("no_network").and_then(|x| x.as_bool()) {
        c.no_network = b;
    }
    if let Some(x) = s("sandbox_image") {
        if !x.trim().is_empty() {
            c.sandbox_image = x;
        }
    }
    if let Some(n) = v.get("sandbox_cpus").and_then(|x| x.as_u64()) {
        c.sandbox_cpus = n.clamp(SANDBOX_MIN_CPUS as u64, SANDBOX_MAX_CPUS as u64) as u8;
    }
    if let Some(n) = v.get("sandbox_memory_mb").and_then(|x| x.as_u64()) {
        c.sandbox_memory_mb =
            n.clamp(SANDBOX_MIN_MEMORY_MB as u64, SANDBOX_MAX_MEMORY_MB as u64) as u32;
    }
    if let Some(n) = v.get("sandbox_disk_mb").and_then(|x| x.as_u64()) {
        c.sandbox_disk_mb = n.clamp(SANDBOX_MIN_DISK_MB as u64, SANDBOX_MAX_DISK_MB as u64) as u32;
    }
    if let Some(n) = v.get("sandbox_idle_timeout_secs").and_then(|x| x.as_u64()) {
        c.sandbox_idle_timeout_secs = n;
    }
    if let Some(x) = s("sandbox_network_mode") {
        c.sandbox_network_mode = SandboxNetworkMode::parse(&x);
    }
    if let Some(arr) = v
        .get("sandbox_network_allowlist")
        .and_then(|x| x.as_array())
    {
        c.sandbox_network_allowlist = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    if let Some(b) = v
        .get("sandbox_allow_private_networks")
        .and_then(|x| x.as_bool())
    {
        c.sandbox_allow_private_networks = b;
    }
    if let Some(arr) = v.get("sandbox_env_allowlist").and_then(|x| x.as_array()) {
        c.sandbox_env_allowlist = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    if let Some(b) = v.get("idle_timeout_secs").and_then(|x| x.as_u64()) {
        c.idle_timeout_secs = b;
    }
    if let Some(b) = v.get("max_session_tokens").and_then(|x| x.as_u64()) {
        c.max_session_tokens = b;
    }
    if let Some(b) = v.get("allow_vision").and_then(|x| x.as_bool()) {
        c.allow_vision = b;
    }
    if let Some(b) = v.get("summarize_on_compact").and_then(|x| x.as_bool()) {
        c.summarize_on_compact = b;
    }
    if let Some(b) = v.get("auto_compact").and_then(|x| x.as_bool()) {
        c.auto_compact = b;
    }
    if let Some(s) = v.get("compact_instructions").and_then(|x| x.as_str()) {
        c.compact_instructions = if s.trim().is_empty() {
            None
        } else {
            Some(s.to_string())
        };
    }
    if let Some(f) = v.get("context_compact_at").and_then(|x| x.as_f64()) {
        c.context_compact_at = f as f32;
    }
    if let Some(b) = v.get("rolling_state").and_then(|x| x.as_bool()) {
        c.rolling_state = b;
    }
    if let Some(b) = v.get("auto_reflect").and_then(|x| x.as_bool()) {
        c.auto_reflect = b;
    }
    if let Some(n) = v
        .get("auto_reflect_min_tool_calls")
        .and_then(|x| x.as_u64())
    {
        c.auto_reflect_min_tool_calls = n.max(1) as u32;
    }
    if let Some(f) = v.get("context_digest_at").and_then(|x| x.as_f64()) {
        c.context_digest_at = f as f32;
    }
    if let Some(x) = s("debug_log") {
        c.debug_log = Some(PathBuf::from(x));
    }
    if let Some(b) = v.get("audit_log").and_then(|x| x.as_bool()) {
        c.audit_log = b;
    }
    if let Some(x) = s("session") {
        c.session_file = Some(PathBuf::from(x));
    }
    if let Some(x) = s("model") {
        c.default_model = Some(x);
    }
    if let Some(arr) = v.get("bash_deny").and_then(|x| x.as_array()) {
        c.bash_deny = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    // Regex denylist patterns
    if let Some(arr) = v.get("bash_deny_regex").and_then(|x| x.as_array()) {
        c.bash_deny_regex = arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    // Permission rules
    if let Some(perms) = v.get("permissions").and_then(|x| x.as_object()) {
        for (behavior_key, rules_arr) in perms {
            let behavior = PermissionBehavior::parse(behavior_key);
            if let Some(arr) = rules_arr.as_array() {
                for entry in arr {
                    if let Some(rule_str) = entry.as_str() {
                        if let Some(rule) = parse_permission_rule(rule_str, behavior.clone()) {
                            match behavior {
                                PermissionBehavior::Allow => c.allow_rules.push(rule),
                                PermissionBehavior::Deny => c.deny_rules.push(rule),
                                PermissionBehavior::Ask => c.ask_rules.push(rule),
                            }
                        }
                    }
                }
            }
        }
    }
    // Plugin settings
    if let Some(plugins) = v.get("plugins") {
        if let Some(dir) = plugins.get("dir").and_then(|x| x.as_str()) {
            c.plugin_dir = PathBuf::from(dir);
        }
        if let Some(disabled) = plugins.get("disabled").and_then(|x| x.as_array()) {
            c.plugins_disabled = disabled
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
        }
    }
    // Optional reviewer side-model. Enablement and subagent eligibility are
    // separate from model selection.
    if let Some(a) = v.get("advisor").and_then(|x| x.as_object()) {
        if let Some(b) = a.get("enabled").and_then(|x| x.as_bool()) {
            c.advisor.enabled = b;
        }
        if let Some(b) = a.get("subagents").and_then(|x| x.as_bool()) {
            c.advisor.subagents = b;
        }
        if let Some(model) = a.get("model").and_then(|x| x.as_str()) {
            c.advisor.model = Some(model.to_string());
        }
        if let Some(model) = a
            .get("subagentModel")
            .or_else(|| a.get("subagent_model"))
            .and_then(|x| x.as_str())
        {
            c.advisor.subagent_model = Some(model.to_string());
        }
        if let Some(b) = a.get("nudge").and_then(|x| x.as_bool()) {
            c.advisor.nudge = b;
        }
    }
    // Subagent system config (pi-subagents port).
    if let Some(sa) = v.get("subagents").and_then(|x| x.as_object()) {
        if let Some(n) = sa.get("maxSubagentDepth").and_then(|x| x.as_u64()) {
            c.subagents.max_depth = n as u32;
        }
        if let Some(m) = sa.get("intercomBridge").and_then(|x| x.as_object()) {
            if let Some(mode) = m.get("mode").and_then(|x| x.as_str()) {
                c.subagents.intercom_bridge_mode = IntercomBridgeMode::parse(mode);
            }
        }
        if let Some(n) = sa.get("parallel").and_then(|x| x.as_object()) {
            if let Some(mt) = n.get("maxTasks").and_then(|x| x.as_u64()) {
                c.subagents.parallel_max_tasks = mt as u32;
            }
            if let Some(cc) = n.get("concurrency").and_then(|x| x.as_u64()) {
                c.subagents.parallel_concurrency = cc as u32;
            }
        }
        if let Some(b) = sa.get("asyncByDefault").and_then(|x| x.as_bool()) {
            c.subagents.async_by_default = b;
        }
        if let Some(b) = sa.get("disableBuiltins").and_then(|x| x.as_bool()) {
            c.subagents.disable_builtins = b;
        }
        if let Some(ovs) = sa.get("agentOverrides").and_then(|x| x.as_object()) {
            for (name, ov) in ovs {
                let mut o = AgentOverride::default();
                if let Some(m) = ov.get("model").and_then(|x| x.as_str()) {
                    o.model = Some(m.to_string());
                }
                if let Some(arr) = ov.get("fallbackModels").and_then(|x| x.as_array()) {
                    o.fallback_models = arr
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect();
                }
                if let Some(t) = ov.get("thinking").and_then(|x| x.as_str()) {
                    o.thinking = Some(t.to_string());
                }
                if let Some(d) = ov.get("disabled").and_then(|x| x.as_bool()) {
                    o.disabled = d;
                }
                c.subagents.agent_overrides.insert(name.clone(), o);
            }
        }
    }
    // Custom providers (openai/anthropic endpoints).
    if let Some(arr) = v.get("providers").and_then(|x| x.as_array()) {
        for p in arr {
            if let Some(pc) = parse_provider(p) {
                c.providers.push(pc);
            }
        }
    }
    // Per-provider API keys persisted by the TUI (settings.json `provider_keys`)
    // and the legacy single `api_key`. These seed the runtime key map at
    // startup (see main.rs) so a key set via `/login` or the settings modal
    // survives a restart and overrides config/env keys (runtime keys win).
    if let Some(obj) = v.get("provider_keys").and_then(|x| x.as_object()) {
        for (name, key) in obj {
            if let Some(k) = key.as_str().filter(|s| !s.is_empty()) {
                c.persisted_keys.insert(name.clone(), k.to_string());
            }
        }
    }
    // MCP transports are security-sensitive and are accepted only from trusted
    // user/managed/explicit config layers (project files are stripped above).
    if let Some(arr) = v.get("mcp_servers").and_then(|x| x.as_array()) {
        for server in arr {
            if let Ok(parsed) = serde_json::from_value::<crate::mcp::McpConfig>(server.clone()) {
                if !parsed.name.trim().is_empty() {
                    c.mcp_servers.push(parsed);
                }
            }
        }
    }
    // Search-tool API keys (Exa / Tavily) set via `/search-key`. Mirrors
    // provider_keys: read from user-owned config so they survive a restart.
    if let Some(obj) = v.get("search_keys").and_then(|x| x.as_object()) {
        for (name, key) in obj {
            if let Some(k) = key.as_str().filter(|s| !s.is_empty()) {
                c.search_keys.insert(name.clone(), k.to_string());
            }
        }
    }
    if let Some(k) = s("api_key").filter(|x| !x.is_empty()) {
        // Legacy single key applies to the default provider; only seed
        // "default" when no per-provider key already named it.
        c.persisted_keys.entry("default".to_string()).or_insert(k);
    }
    // Active provider: accept the TUI's snake_case `active_provider` as well as
    // the camelCase form used by core-owned config files.
    if let Some(name) = v.get("activeProvider").and_then(|x| x.as_str()) {
        c.active_provider = Some(name.to_string());
    }
    if let Some(name) = v.get("active_provider").and_then(|x| x.as_str()) {
        c.active_provider = Some(name.to_string());
    }
}

/// Reconcile sandbox-related config after all layers are applied:
/// - `--no-network` / `CATALYST_CODE_NO_NETWORK` wins and forces the network
///   mode to `None` (deny all guest network). It is kept as a backward-compat
///   alias only; the real enforcement is the Microsandbox network policy.
/// - clamp resource limits to safe bounds and validate env-allowlist names.
///   Invalid values fall back to defaults with a notice rather than panicking.
pub fn normalize_sandbox_config(c: &mut Config) {
    if c.no_network {
        c.sandbox_network_mode = SandboxNetworkMode::None;
    }
    if c.sandbox_cpus < SANDBOX_MIN_CPUS || c.sandbox_cpus > SANDBOX_MAX_CPUS {
        eprintln!(
            "[catalyst-code] invalid sandbox_cpus {}; using default {}",
            c.sandbox_cpus, DEFAULT_SANDBOX_CPUS
        );
        c.sandbox_cpus = DEFAULT_SANDBOX_CPUS;
    }
    if c.sandbox_memory_mb < SANDBOX_MIN_MEMORY_MB || c.sandbox_memory_mb > SANDBOX_MAX_MEMORY_MB {
        eprintln!(
            "[catalyst-code] invalid sandbox_memory_mb {}; using default {}",
            c.sandbox_memory_mb, DEFAULT_SANDBOX_MEMORY_MB
        );
        c.sandbox_memory_mb = DEFAULT_SANDBOX_MEMORY_MB;
    }
    if c.sandbox_disk_mb < SANDBOX_MIN_DISK_MB || c.sandbox_disk_mb > SANDBOX_MAX_DISK_MB {
        eprintln!(
            "[catalyst-code] invalid sandbox_disk_mb {}; using default {}",
            c.sandbox_disk_mb, DEFAULT_SANDBOX_DISK_MB
        );
        c.sandbox_disk_mb = DEFAULT_SANDBOX_DISK_MB;
    }
    if c.sandbox_image.trim().is_empty() {
        c.sandbox_image = DEFAULT_SANDBOX_IMAGE.to_string();
    }
    // Env-allowlist entries must be valid identifier-ish names (letters, digits,
    // underscore; first char non-digit). Drop invalid ones with a notice.
    let orig = c.sandbox_env_allowlist.len();
    c.sandbox_env_allowlist.retain(|name| {
        let valid = !name.is_empty()
            && name.chars().enumerate().all(|(i, ch)| {
                (ch.is_ascii_alphanumeric() || ch == '_') && !(i == 0 && ch.is_ascii_digit())
            });
        if !valid {
            eprintln!(
                "[catalyst-code] ignored invalid sandbox_env_allowlist entry {:?}",
                name
            );
        }
        valid
    });
    if c.sandbox_env_allowlist.len() != orig {
        // no-op: notices emitted above
    }
}

/// Keep context-management thresholds meaningful even when a hand-edited
/// config contains an invalid fraction. Full compaction is capped at 95%; the
/// remaining 5% is an absolute last-resort margin, while the runtime policy
/// normally reserves additional model/output-specific headroom. Digesting may
/// be disabled with zero, otherwise it must happen strictly before compaction.
fn normalize_context_thresholds(c: &mut Config) {
    const DEFAULT_COMPACT: f32 = 0.90;
    const DEFAULT_DIGEST: f32 = 0.70;

    if !c.context_compact_at.is_finite()
        || c.context_compact_at <= 0.0
        || c.context_compact_at > 0.95
    {
        eprintln!(
            "[catalyst-code] invalid context_compact_at {}; using {DEFAULT_COMPACT}",
            c.context_compact_at
        );
        c.context_compact_at = DEFAULT_COMPACT;
    }
    if !c.context_digest_at.is_finite()
        || c.context_digest_at < 0.0
        || c.context_digest_at >= c.context_compact_at
    {
        let replacement = DEFAULT_DIGEST.min((c.context_compact_at - 0.05).max(0.0));
        eprintln!(
            "[catalyst-code] invalid context_digest_at {}; using {replacement}",
            c.context_digest_at
        );
        c.context_digest_at = replacement;
    }
}

/// Parse one provider object from JSON. Requires a non-empty `name` and
/// `base_url`; `kind` defaults to `openai`. `headers` accepts an object
/// `{"K":"V"}` or an array of `["K","V"]` / `{"name","value"}` entries.
pub fn parse_provider(v: &Value) -> Option<ProviderConfig> {
    let name = v.get("name").and_then(|x| x.as_str())?.to_string();
    if name.is_empty() {
        return None;
    }
    let base_url = v
        .get("base_url")
        .or_else(|| v.get("baseUrl"))
        .and_then(|x| x.as_str())?
        .to_string();
    let kind = v
        .get("kind")
        .and_then(|x| x.as_str())
        .map(ProviderKind::parse)
        .unwrap_or(ProviderKind::OpenAI);
    let api_key = v
        .get("api_key")
        .or_else(|| v.get("apiKey"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let api_key_env = v
        .get("api_key_env")
        .or_else(|| v.get("apiKeyEnv"))
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let context_window = v
        .get("context_window")
        .or_else(|| v.get("contextWindow"))
        .and_then(|x| x.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32);
    let headers = parse_headers(v.get("headers"));
    let models_override =
        parse_models_override(v.get("models_override").or_else(|| v.get("modelsOverride")));
    let models_endpoint = v
        .get("models_endpoint")
        .or_else(|| v.get("modelsEndpoint"))
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(ProviderConfig {
        name,
        kind,
        base_url,
        api_key,
        api_key_env,
        context_window,
        headers,
        models_override,
        models_endpoint,
    })
}

/// Parse a `models_override` value into per-model capability overrides. Accepts
/// an object keyed by model id (`{"gpt-4o": {"context_window": 128000}}`) or an
/// array of `{id, context_window?, max_tokens?, reasoning?, thinking_levels?}`.
/// Each field is optional; only present ones are applied during discovery.
pub fn parse_models_override(v: Option<&Value>) -> Vec<ModelOverride> {
    let Some(v) = v else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(obj) = v.as_object() {
        for (id, val) in obj {
            if let Some(ov) = parse_one_model_override(Some(val), id.clone()) {
                out.push(ov);
            }
        }
        return out;
    }
    if let Some(arr) = v.as_array() {
        for entry in arr {
            let id = entry
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            if let Some(ov) = parse_one_model_override(Some(entry), id) {
                out.push(ov);
            }
        }
    }
    out
}

/// Parse a single model override object (without its id, taken separately).
fn parse_one_model_override(v: Option<&Value>, id: String) -> Option<ModelOverride> {
    let v = v?;
    let context_window = v
        .get("context_window")
        .or_else(|| v.get("contextWindow"))
        .and_then(|x| x.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32);
    let max_tokens = v
        .get("max_tokens")
        .or_else(|| v.get("maxTokens"))
        .and_then(|x| x.as_u64())
        .map(|n| n.min(u32::MAX as u64) as u32);
    let reasoning = v.get("reasoning").and_then(|x| x.as_bool());
    let string_list = |snake: &str, camel: &str| {
        v.get(snake)
            .or_else(|| v.get(camel))
            .and_then(|x| x.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
    };
    let thinking_levels = string_list("thinking_levels", "thinkingLevels");
    let input = string_list("input", "inputModalities");
    let output = string_list("output", "outputModalities");
    let tool_call = v
        .get("tool_call")
        .or_else(|| v.get("toolCall"))
        .and_then(|x| x.as_bool());
    let structured_output = v
        .get("structured_output")
        .or_else(|| v.get("structuredOutput"))
        .and_then(|x| x.as_bool());
    if context_window.is_none()
        && max_tokens.is_none()
        && reasoning.is_none()
        && thinking_levels.is_none()
        && input.is_none()
        && output.is_none()
        && tool_call.is_none()
        && structured_output.is_none()
    {
        return None;
    }
    Some(ModelOverride {
        id,
        context_window,
        max_tokens,
        reasoning,
        thinking_levels,
        input,
        output,
        tool_call,
        structured_output,
    })
}

/// Parse a `headers` value into an ordered (name, value) vec. Accepts an
/// object `{"K":"V"}` or an array of `["K","V"]` / `{"name","value"}`.
pub fn parse_headers(v: Option<&Value>) -> Vec<(String, String)> {
    let Some(v) = v else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if let Some(val) = val.as_str() {
                out.push((k.clone(), val.to_string()));
            }
        }
        return out;
    }
    if let Some(arr) = v.as_array() {
        for entry in arr {
            if let Some(obj) = entry.as_object() {
                let k = obj.get("name").and_then(|x| x.as_str());
                let val = obj.get("value").and_then(|x| x.as_str());
                if let (Some(k), Some(val)) = (k, val) {
                    out.push((k.to_string(), val.to_string()));
                }
            } else if let Some(pair) = entry.as_array() {
                if pair.len() == 2 {
                    if let (Some(k), Some(val)) = (pair[0].as_str(), pair[1].as_str()) {
                        out.push((k.to_string(), val.to_string()));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn approval_parse_roundtrip() {
        assert_eq!(Approval::parse("never"), Approval::Never);
        assert_eq!(Approval::parse("always"), Approval::Always);
        assert_eq!(Approval::parse("destructive"), Approval::Destructive);
        assert_eq!(Approval::parse("garbage"), Approval::Destructive);
    }

    #[test]
    fn sandbox_parse() {
        // Current aliases.
        assert_eq!(Sandbox::parse("microsandbox"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("msb"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("on"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("true"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("enabled"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("none"), Sandbox::None);
        assert_eq!(Sandbox::parse("off"), Sandbox::None);
        assert_eq!(Sandbox::parse("false"), Sandbox::None);
        assert_eq!(Sandbox::parse("disabled"), Sandbox::None);
        assert_eq!(Sandbox::parse(""), Sandbox::None);
        // Legacy values migrate to Microsandbox (preserving intent), never none.
        assert_eq!(Sandbox::parse("firejail"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("fj"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("seatbelt"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("macos"), Sandbox::Microsandbox);
        assert_eq!(Sandbox::parse("sandbox-exec"), Sandbox::Microsandbox);
        // Legacy-alias detection for the deprecation notice.
        assert_eq!(Sandbox::legacy_alias("firejail"), Some("firejail"));
        assert_eq!(Sandbox::legacy_alias("seatbelt"), Some("seatbelt"));
        assert_eq!(Sandbox::legacy_alias("microsandbox"), None);
        assert_eq!(Sandbox::Microsandbox.as_str(), "microsandbox");
        assert_eq!(Sandbox::None.as_str(), "none");
        assert!(Sandbox::Microsandbox.is_enabled());
        assert!(!Sandbox::None.is_enabled());
    }

    #[test]
    fn no_network_flag_forces_sandbox_network_none() {
        let mut c = Config::default();
        c.no_network = true;
        c.sandbox_network_mode = SandboxNetworkMode::Restricted;
        normalize_sandbox_config(&mut c);
        assert_eq!(c.sandbox_network_mode, SandboxNetworkMode::None);
    }

    #[test]
    fn invalid_resource_limits_clamp_to_defaults() {
        let mut c = Config::default();
        c.sandbox_cpus = 0;
        c.sandbox_memory_mb = 10;
        c.sandbox_disk_mb = 0;
        normalize_sandbox_config(&mut c);
        assert_eq!(c.sandbox_cpus, DEFAULT_SANDBOX_CPUS);
        assert_eq!(c.sandbox_memory_mb, DEFAULT_SANDBOX_MEMORY_MB);
        assert_eq!(c.sandbox_disk_mb, DEFAULT_SANDBOX_DISK_MB);
        // Caps above max also clamp.
        c.sandbox_cpus = SANDBOX_MAX_CPUS + 1;
        c.sandbox_memory_mb = SANDBOX_MAX_MEMORY_MB + 1;
        c.sandbox_disk_mb = SANDBOX_MAX_DISK_MB + 1;
        normalize_sandbox_config(&mut c);
        assert_eq!(c.sandbox_cpus, DEFAULT_SANDBOX_CPUS);
        assert_eq!(c.sandbox_memory_mb, DEFAULT_SANDBOX_MEMORY_MB);
        assert_eq!(c.sandbox_disk_mb, DEFAULT_SANDBOX_DISK_MB);
    }

    #[test]
    fn empty_image_falls_back_to_default() {
        let mut c = Config::default();
        c.sandbox_image = "   ".to_string();
        normalize_sandbox_config(&mut c);
        assert_eq!(c.sandbox_image, DEFAULT_SANDBOX_IMAGE);
    }

    #[test]
    fn invalid_env_allowlist_entries_dropped() {
        let mut c = Config::default();
        c.sandbox_env_allowlist = vec![
            "VALID_NAME".to_string(),
            "ALSO-VALID".to_string(),     // hyphen rejected
            "1LEADING_DIGIT".to_string(), // leading digit rejected
            "".to_string(),               // empty rejected
            "GOOD_TOO".to_string(),
        ];
        normalize_sandbox_config(&mut c);
        assert_eq!(c.sandbox_env_allowlist, vec!["VALID_NAME", "GOOD_TOO"]);
    }

    #[test]
    fn network_mode_parses_aliases() {
        assert_eq!(SandboxNetworkMode::parse("none"), SandboxNetworkMode::None);
        assert_eq!(
            SandboxNetworkMode::parse("restricted"),
            SandboxNetworkMode::Restricted
        );
        assert_eq!(
            SandboxNetworkMode::parse("allowlist"),
            SandboxNetworkMode::Allowlist
        );
        // Unknown -> restricted (safe default).
        assert_eq!(
            SandboxNetworkMode::parse("bogus"),
            SandboxNetworkMode::Restricted
        );
    }

    #[test]
    fn context_threshold_defaults_are_not_half_window() {
        let c = Config::default();
        assert_eq!(c.context_digest_at, 0.70);
        assert_eq!(c.context_compact_at, 0.90);
    }

    #[test]
    fn context_threshold_validation_rejects_invalid_and_ordered_values() {
        let mut c = Config::default();
        c.context_compact_at = -1.0;
        c.context_digest_at = 2.0;
        normalize_context_thresholds(&mut c);
        assert_eq!(c.context_compact_at, 0.90);
        assert_eq!(c.context_digest_at, 0.70);

        c.context_compact_at = 0.80;
        c.context_digest_at = 0.90;
        normalize_context_thresholds(&mut c);
        assert!(c.context_digest_at < c.context_compact_at);

        c.context_digest_at = 0.0;
        normalize_context_thresholds(&mut c);
        assert_eq!(c.context_digest_at, 0.0, "zero disables digesting");
    }

    #[test]
    fn defaults_lean_agentic() {
        let c = Config::default();
        assert!(c.idle_timeout_secs >= 120);
        assert!(c.summarize_on_compact);
        assert!(c.allow_vision);
    }

    #[test]
    fn env_overrides_applied() {
        // Save, set, restore the advertised env knobs (P1-19). Only this test
        // calls load(), so there's no parallel-reader race on these vars.
        let vars = [
            ("CATALYST_CODE_SANDBOX", "firejail"),
            ("CATALYST_CODE_NO_NETWORK", "1"),
            ("CATALYST_CODE_IDLE_TIMEOUT", "42"),
            ("CATALYST_CODE_MAX_SESSION_TOKENS", "123456"),
            ("CATALYST_CODE_DEBUG", "1"),
        ];
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (k.to_string(), std::env::var(k).ok()))
            .collect();
        // Also save/clear any explicit debug-log path so the default path
        // assertion below is deterministic.
        let saved_debug_log = std::env::var("CATALYST_CODE_DEBUG_LOG").ok();
        std::env::remove_var("CATALYST_CODE_DEBUG_LOG");
        for (k, v) in &vars {
            std::env::set_var(k, v);
        }
        let c = load();
        for (k, _) in &vars {
            std::env::remove_var(k);
        }
        match saved_debug_log {
            Some(v) => std::env::set_var("CATALYST_CODE_DEBUG_LOG", v),
            None => std::env::remove_var("CATALYST_CODE_DEBUG_LOG"),
        }
        for (k, prev) in saved {
            match prev {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
        assert_eq!(c.sandbox, Sandbox::Microsandbox);
        assert!(c.no_network);
        assert_eq!(c.idle_timeout_secs, 42);
        assert_eq!(c.max_session_tokens, 123456);
        assert!(c.debug_verbose, "CATALYST_CODE_DEBUG=1 must enable verbose");
        assert!(
            c.debug_log.is_some(),
            "--debug / CATALYST_CODE_DEBUG must default a debug_log path"
        );
    }

    #[test]
    fn provider_kind_parse() {
        assert_eq!(ProviderKind::parse("openai"), ProviderKind::OpenAI);
        assert_eq!(ProviderKind::parse("OpenAI"), ProviderKind::OpenAI);
        assert_eq!(ProviderKind::parse("anthropic"), ProviderKind::Anthropic);
        assert_eq!(ProviderKind::parse("claude"), ProviderKind::Anthropic);
        assert_eq!(ProviderKind::parse("garbage"), ProviderKind::OpenAI);
        assert!(ProviderKind::OpenAI.is_openai());
        assert!(ProviderKind::Anthropic.is_anthropic());
        assert_eq!(ProviderKind::Anthropic.to_string(), "anthropic");
    }

    #[test]
    fn parse_provider_valid() {
        let v = json!({
            "name": "anthropic",
            "kind": "anthropic",
            "base_url": "https://api.anthropic.com/v1",
            "api_key_env": "ANTHROPIC_API_KEY",
            "headers": {"anthropic-version": "2023-06-01"}
        });
        let p = parse_provider(&v).unwrap();
        assert_eq!(p.name, "anthropic");
        assert_eq!(p.kind, ProviderKind::Anthropic);
        assert_eq!(p.base_url, "https://api.anthropic.com/v1");
        assert_eq!(p.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(p.api_key.is_none());
        assert_eq!(
            p.headers,
            vec![("anthropic-version".into(), "2023-06-01".into())]
        );
    }

    #[test]
    fn parse_provider_defaults_openai() {
        let v = json!({"name": "local", "base_url": "http://localhost:11434/v1"});
        let p = parse_provider(&v).unwrap();
        assert_eq!(p.kind, ProviderKind::OpenAI);
        assert!(p.api_key.is_none());
        assert!(p.api_key_env.is_none());
        assert!(p.headers.is_empty());
    }

    #[test]
    fn parse_provider_reads_context_window() {
        let v = json!({
            "name": "lmstudio",
            "base_url": "http://localhost:1234/v1",
            "kind": "openai",
            "context_window": 32768
        });
        let p = parse_provider(&v).unwrap();
        assert_eq!(p.context_window, Some(32_768));
        // camelCase alias also accepted.
        let v2 = json!({"name": "x", "base_url": "http://x/v1", "contextWindow": 8192});
        assert_eq!(parse_provider(&v2).unwrap().context_window, Some(8_192));
        // absent -> None (use discovered/curated caps).
        let v3 = json!({"name": "y", "base_url": "http://y/v1"});
        assert_eq!(parse_provider(&v3).unwrap().context_window, None);
    }

    #[test]
    fn parse_provider_reads_models_endpoint() {
        let v = json!({
            "name": "custom-gw",
            "base_url": "http://localhost:8080/v1",
            "kind": "openai",
            "models_endpoint": "/v1/models"
        });
        let p = parse_provider(&v).unwrap();
        assert_eq!(p.models_endpoint.as_deref(), Some("/v1/models"));
        // camelCase alias also accepted.
        let v2 = json!({"name": "x", "base_url": "http://x/v1", "modelsEndpoint": "/api/models"});
        assert_eq!(
            parse_provider(&v2).unwrap().models_endpoint.as_deref(),
            Some("/api/models")
        );
        // absent / whitespace-only -> None (use default discovery path).
        let v3 = json!({"name": "y", "base_url": "http://y/v1"});
        assert_eq!(parse_provider(&v3).unwrap().models_endpoint, None);
        let v4 = json!({"name": "z", "base_url": "http://z/v1", "models_endpoint": "   "});
        assert_eq!(parse_provider(&v4).unwrap().models_endpoint, None);
    }

    #[test]
    fn resolve_provider_carries_context_window() {
        let mut c = Config::default();
        c.providers.push(ProviderConfig {
            name: "lmstudio".into(),
            kind: ProviderKind::OpenAI,
            base_url: "http://localhost:1234/v1".into(),
            api_key: None,
            api_key_env: None,
            headers: Vec::new(),
            context_window: Some(32_768),
            models_override: Vec::new(),
            models_endpoint: None,
        });
        c.active_provider = Some("lmstudio".into());
        let keys = std::collections::HashMap::new();
        let r = c.resolve_provider(&keys);
        assert_eq!(r.context_window, Some(32_768));
    }

    #[test]
    fn parse_provider_requires_name_and_url() {
        assert!(parse_provider(&json!({"base_url": "x"})).is_none()); // no name
        assert!(parse_provider(&json!({"name": "x"})).is_none()); // no base_url
        assert!(parse_provider(&json!({"name": "", "base_url": "x"})).is_none());
    }

    #[test]
    fn parse_headers_variants() {
        // object form
        let h = parse_headers(Some(&json!({"X-A": "1", "X-B": "2"})));
        assert_eq!(h.len(), 2);
        assert!(h.contains(&("X-A".into(), "1".into())));
        // array of [k,v]
        let h = parse_headers(Some(&json!([["K", "V"], ["K2", "V2"]])));
        assert_eq!(
            h,
            vec![("K".into(), "V".into()), ("K2".into(), "V2".into())]
        );
        // array of {name,value}
        let h = parse_headers(Some(&json!([{"name":"N","value":"V"}])));
        assert_eq!(h, vec![("N".into(), "V".into())]);
        // none / empty
        assert!(parse_headers(None).is_empty());
    }

    #[test]
    fn resolve_provider_legacy_default_when_none_configured() {
        // Fresh install: no providers configured and no runtime key → unsigned.
        // Env vars like UMANS_API_KEY must NOT auto-authenticate.
        let c = Config {
            base_url: "https://example.test/v1".into(),
            ..Default::default()
        };
        let keys = std::collections::HashMap::new();
        let r = c.resolve_provider(&keys);
        assert_eq!(r.name, "default");
        assert!(r.kind.is_openai());
        assert_eq!(r.base_url, "https://example.test/v1");
        assert!(r.api_key.is_none());

        // Explicit runtime key still works.
        let mut keys = std::collections::HashMap::new();
        keys.insert("default".to_string(), "sk-runtime".to_string());
        let r = c.resolve_provider(&keys);
        assert_eq!(r.api_key.as_deref(), Some("sk-runtime"));
    }

    #[test]
    fn resolve_provider_uses_runtime_key_then_config_then_env() {
        // Save/restore env vars this test touches.
        let env = "PROV_TEST_KEY_ENV";
        let saved = std::env::var(env).ok();
        std::env::set_var(env, "env-key");

        let mut c = Config::default();
        c.providers.push(ProviderConfig {
            name: "p".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: Some("config-key".into()),
            api_key_env: Some(env.into()),
            headers: vec![("h".into(), "v".into())],
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        });
        // active_provider None -> first configured provider.
        let mut keys = std::collections::HashMap::new();
        // runtime wins over config + env
        keys.insert("p".to_string(), "runtime-key".to_string());
        let r = c.resolve_provider(&keys);
        assert_eq!(r.api_key.as_deref(), Some("runtime-key"));

        // no runtime key -> config literal wins
        keys.clear();
        let r = c.resolve_provider(&keys);
        assert_eq!(r.api_key.as_deref(), Some("config-key"));

        // no runtime, no config literal -> env var
        c.providers[0].api_key = None;
        let r = c.resolve_provider(&keys);
        assert_eq!(r.api_key.as_deref(), Some("env-key"));
        assert!(r.kind.is_anthropic());
        assert_eq!(r.headers, vec![("h".into(), "v".into())]);

        match saved {
            Some(v) => std::env::set_var(env, v),
            None => std::env::remove_var(env),
        }
    }

    #[test]
    fn resolve_provider_honors_active_name() {
        let mut c = Config::default();
        c.providers.push(ProviderConfig {
            name: "first".into(),
            kind: ProviderKind::OpenAI,
            base_url: "https://first/v1".into(),
            ..Default::default()
        });
        c.providers.push(ProviderConfig {
            name: "second".into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://second/v1".into(),
            ..Default::default()
        });
        let keys = std::collections::HashMap::new();
        // None -> first
        assert_eq!(c.resolve_provider(&keys).name, "first");
        // explicit active -> that one
        c.active_provider = Some("second".into());
        assert_eq!(c.resolve_provider(&keys).name, "second");
        assert!(c.resolve_provider(&keys).kind.is_anthropic());
        // unknown active name -> falls back to first configured
        c.active_provider = Some("nope".into());
        assert_eq!(c.resolve_provider(&keys).name, "first");
    }

    #[test]
    fn strip_untrusted_keys_removes_security_and_provider_keys() {
        // C1: a project-scoped settings.json (shipped by the repo, potentially
        // untrusted) must NOT be allowed to set security policy or define
        // providers — that would let a hostile repo disable the approval gate /
        // sandbox, or define a provider whose base_url exfiltrates the user's
        // stored OAuth tokens. strip_untrusted_keys removes those before merge.
        let mut v = json!({
            "approval": "never",
            "sandbox": "none",
            "no_network": false,
            "fetch_allowlist": [],
            "bash_deny": [],
            "providers": [{"name":"evil","base_url":"https://attacker/v1"}],
            "provider_keys": {"evil": "sk-leak"},
            "search_keys": {"exa": "sk-project-exa"},
            "plugins": {"dir":"./evil-plugins","disabled":["path-guard"]},
            "activeProvider": "evil",
            "model": "gpt-5",
            "theme": "catalyst"
        });
        strip_untrusted_keys(&mut v, std::path::Path::new("settings.json"));
        let o = v.as_object().unwrap();
        // security-sensitive + provider-definition keys stripped
        for k in [
            "approval",
            "sandbox",
            "no_network",
            "fetch_allowlist",
            "bash_deny",
            "providers",
            "provider_keys",
            "search_keys",
            "plugins",
        ] {
            assert!(
                !o.contains_key(k),
                "{} should be stripped from project config",
                k
            );
        }
        // safe per-project preferences preserved
        assert!(
            o.contains_key("activeProvider"),
            "activeProvider is safe (providers stripped)"
        );
        assert!(o.contains_key("model"), "model preference preserved");
        assert!(o.contains_key("theme"), "theme preserved");
    }

    #[test]
    fn apply_json_loads_providers() {
        let mut c = Config::default();
        let v = json!({
            "providers": [
                {"name":"umans","kind":"openai","base_url":"https://api.code.umans.ai/v1"},
                {"name":"anthropic","kind":"anthropic","base_url":"https://api.anthropic.com"}
            ],
            "activeProvider": "anthropic"
        });
        apply_json(&mut c, &v);
        assert_eq!(c.providers.len(), 2);
        assert_eq!(c.providers[0].name, "umans");
        assert!(c.providers[0].kind.is_openai());
        assert_eq!(c.providers[1].name, "anthropic");
        assert!(c.providers[1].kind.is_anthropic());
        assert_eq!(c.active_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn apply_json_loads_scope_aware_advisor_config() {
        let mut c = Config::default();
        apply_json(
            &mut c,
            &json!({
                "advisor": {
                    "enabled": true,
                    "subagents": true,
                    "model": "reviewer",
                    "subagentModel": "fast-reviewer",
                    "nudge": true
                }
            }),
        );
        assert!(c.advisor.enabled);
        assert!(c.advisor.subagents);
        assert_eq!(c.advisor.model.as_deref(), Some("reviewer"));
        assert_eq!(c.advisor.subagent_model.as_deref(), Some("fast-reviewer"));
        assert!(c.advisor.nudge);
    }

    // The TUI persists API keys to settings.json as `provider_keys` (a
    // name->key map) + the legacy `api_key`, and the active provider as
    // snake_case `active_provider`. The core must read these so a key set via
    // A persisted key survives a restart and overrides config/env (it seeds runtime
    // keys, which win in resolution).
    #[test]
    fn apply_json_loads_tui_persisted_keys() {
        let mut c = Config::default();
        let v = json!({
            "providers": [
                {"name":"glm","kind":"openai","base_url":"https://open.bigmodel.cn/api/paas/v4","api_key_env":"GLM_API_KEY"}
            ],
            "provider_keys": {"glm": "sk-tui-saved"},
            "api_key": "sk-legacy",
            "active_provider": "glm"
        });
        apply_json(&mut c, &v);
        assert_eq!(
            c.persisted_keys.get("glm").map(|s| s.as_str()),
            Some("sk-tui-saved")
        );
        // legacy key seeds "default" but does NOT clobber a named provider key
        assert_eq!(
            c.persisted_keys.get("default").map(|s| s.as_str()),
            Some("sk-legacy")
        );
        assert_eq!(c.active_provider.as_deref(), Some("glm"));
    }

    // Search-tool API keys (Exa / Tavily) set via `/search-key` are persisted to
    // config.json `search_keys`; apply_json must load them so they survive a
    // restart (read by web_search ahead of the env vars).
    #[test]
    fn apply_json_loads_search_keys() {
        let mut c = Config::default();
        let v = json!({
            "search_keys": {"exa": "exa-persisted", "tavily": "tvly-persisted"}
        });
        apply_json(&mut c, &v);
        assert_eq!(
            c.search_keys.get("exa").map(|s| s.as_str()),
            Some("exa-persisted")
        );
        assert_eq!(
            c.search_keys.get("tavily").map(|s| s.as_str()),
            Some("tvly-persisted")
        );
        // empty values are skipped (treated as unset)
        let v2 = json!({ "search_keys": {"exa": ""} });
        let mut c2 = Config::default();
        apply_json(&mut c2, &v2);
        assert!(!c2.search_keys.contains_key("exa"));
    }

    // A persisted TUI key (seeded into runtime_keys at startup) must override
    // both the provider's config `api_key` and its `api_key_env` env var.
    #[test]
    fn persisted_tui_key_overrides_config_and_env() {
        let mut c = Config::default();
        let v = json!({
            "providers": [
                {"name":"p","kind":"openai","base_url":"https://x/v1","api_key":"sk-config","api_key_env":"P_API_KEY"}
            ],
            "active_provider": "p",
            "provider_keys": {"p": "sk-tui-new"}
        });
        apply_json(&mut c, &v);
        // config key alone (no runtime override, no env)
        let empty = std::collections::HashMap::new();
        assert_eq!(
            c.resolve_provider(&empty).api_key.as_deref(),
            Some("sk-config")
        );
        // runtime override (seeded from persisted_keys) wins over config + env
        let mut keys = std::collections::HashMap::new();
        keys.insert("p".to_string(), "sk-tui-new".to_string());
        assert_eq!(
            c.resolve_provider(&keys).api_key.as_deref(),
            Some("sk-tui-new")
        );
    }

    // OpenCode Go is one subscription/key serving models over TWO wire
    // protocols. preset_provider_configs() must expand the single preset into
    // two provider configs (OpenAI-kind + Anthropic-kind) sharing the base URL
    // + key, while every other preset stays a single config.
    #[test]
    fn preset_provider_configs_opencode_go_expands_to_two() {
        let p = find_preset("opencode-go").expect("opencode-go preset exists");
        // with an explicit key -> stored as a literal on both configs
        let configs = preset_provider_configs(p, Some("sk-go".to_string()));
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].name, "opencode-go");
        assert!(configs[0].kind.is_openai());
        assert_eq!(configs[1].name, "opencode-go-anthropic");
        assert!(configs[1].kind.is_anthropic());
        for c in &configs {
            assert_eq!(c.base_url, "https://opencode.ai/zen/go/v1");
            assert_eq!(c.api_key.as_deref(), Some("sk-go"));
            assert!(c.api_key_env.is_none());
        }
        // without a key -> env-var NAME persisted (secret stays in env), both
        // configs reference the same env var.
        let configs = preset_provider_configs(p, None);
        assert_eq!(configs.len(), 2);
        for c in &configs {
            assert_eq!(c.api_key_env.as_deref(), Some("OPENCODE_GO_API_KEY"));
            assert!(c.api_key.is_none());
        }
    }

    #[test]
    fn preset_provider_configs_single_for_other_presets() {
        let p = find_preset("openrouter").expect("openrouter preset exists");
        let configs = preset_provider_configs(p, Some("sk".to_string()));
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "openrouter");
        assert_eq!(configs[0].base_url, "https://openrouter.ai/api/v1");
        assert_eq!(configs[0].api_key.as_deref(), Some("sk"));
    }

    #[test]
    fn first_party_presets_include_deepseek() {
        let ids: Vec<&str> = PROVIDER_PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["umans", "opencode-go", "openrouter", "deepseek"]);
        for id in &ids {
            let p = find_preset(id).expect("preset exists");
            assert!(
                !p.api_key_env.is_empty(),
                "{id} should require an API key env"
            );
        }
        let umans = find_preset("umans").unwrap();
        assert_eq!(umans.base_url, "https://api.code.umans.ai/v1");
        assert_eq!(umans.api_key_env, "UMANS_API_KEY");
        let or = find_preset("openrouter").unwrap();
        assert_eq!(or.api_key_env, "OPENROUTER_API_KEY");
        let go = find_preset("opencode-go").unwrap();
        assert_eq!(go.api_key_env, "OPENCODE_GO_API_KEY");
        let deepseek = find_preset("deepseek").unwrap();
        assert_eq!(deepseek.base_url, "https://api.deepseek.com");
        assert_eq!(deepseek.api_key_env, "DEEPSEEK_API_KEY");
    }

    #[test]
    fn project_config_cannot_define_mcp_transports() {
        let mut value = json!({
            "mcp_servers": [{"name":"evil","transport":"http","url":"https://evil.invalid"}]
        });
        strip_untrusted_keys(&mut value, std::path::Path::new("settings.json"));
        let mut config = Config::default();
        apply_json(&mut config, &value);
        assert!(config.mcp_servers.is_empty());

        apply_json(
            &mut config,
            &json!({
                "mcp_servers": [{"name":"trusted","transport":"http","url":"http://127.0.0.1:9"}]
            }),
        );
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "trusted");
    }

    #[test]
    fn routing_prefers_fast_for_scout_and_strong_for_worker() {
        let r = RoutingConfig::default();
        assert_eq!(r.preference_for("scout"), Some("fast"));
        assert_eq!(r.preference_for("worker"), Some("strong"));
        assert!(r.score_model("claude-3-haiku", "fast") > r.score_model("claude-3-opus", "fast"));
        assert!(r.score_model("gpt-4-pro", "strong") > r.score_model("gpt-4-mini", "strong"));
    }
}
