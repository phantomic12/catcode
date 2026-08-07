// Plugin system: self-bootstrapping hooks loaded from .catalyst-code/plugins/.
// Each plugin is a subdirectory with a plugin.json manifest and hook scripts.
// Hooks are spawned as subprocesses with stdin JSON context, stdout JSON response.
// Broken hooks never crash the core; timeouts and parse failures are handled gracefully.
use crate::config::{ProviderConfig, ProviderKind, ResolvedProvider};
use crate::oauth::{LoginOutcome, OAuthPrompt, PendingOauth};
use crate::tools::{Outcome, ToolKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

// ---- constants ----

/// Short plugins pointer injected into the standing system prompt. The full
/// authoring contract lives in the opt-in `plugin-authoring` skill so everyday
/// coding turns do not pay for a ~6k-token manual.
pub const PLUGIN_DOCS: &str = r#"## Plugins

Extend the harness via `.catalyst-code/plugins/` (hooks, custom tools, OAuth,
memory backends, system-prompt injection). Manage with `/plugin-install` /
`/plugin-enable` / `/plugin-disable` / `/plugin-reload` (or the matching
protocol commands). There is no built-in `plugin` tool — use slash/protocol.

When authoring or debugging a plugin, apply the `plugin-authoring` skill
(`/skill:plugin-authoring`) — do not invent schema from memory. Full contract:
hooks (incl. pre_tool/post_tool catch-alls + agent-loop hooks), tools,
overrides, OAuth, and memory providers.
"#;

/// Valid hook point names. Plugins can register for any of these.
pub const HOOK_POINTS: &[&str] = &[
    "pre_bash",
    "pre_write",
    "pre_read",
    "post_bash",
    "post_write",
    "post_read",
    "session_start",
    "session_stop",
    "pre_compact",
    "pre_turn",
    // Agent-loop hooks (PI parity): input/prompt/context/turn boundaries.
    "pre_input",
    "pre_agent_start",
    "pre_context",
    "turn_start",
    "turn_end",
    "session_shutdown",
    // Catch-all hooks that fire for EVERY tool call (in addition to the
    // specific pre_bash/pre_write/pre_read). They cover tools with no
    // dedicated hook (memory, todo_write, git_*, subagent, plugin tools, …)
    // so a plugin can audit/modify/deny ANY tool — the same reach a core edit
    // of the dispatch loop has. pre_tool runs after the specific pre-hook;
    // post_tool runs after the specific post-hook.
    "pre_tool",
    "post_tool",
];

/// Per-hook policy: safety rule, default timeout, and which response fields are honored.
#[derive(Debug, Clone, Copy)]
pub struct HookPolicy {
    pub default_timeout_ms: u64,
    /// What happens when the hook fails (non-zero, timeout, parse fail).
    pub fail_mode: HookFailMode,
    /// Whether `allow:false` is honored (pre_* style). When false, `allow` is ignored.
    pub honor_allow: bool,
    /// Whether `modify` keys are applied. Lifecycle hooks that only emit side effects
    /// may still honor modify for prompt/context surgery.
    pub honor_modify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookFailMode {
    /// Block the operation (pre_* style).
    Deny,
    /// Silently skip the hook result (post_* and advisory lifecycle hooks).
    Skip,
}

/// Policy table for every hook point. This is the single source of truth for
/// how execute_hook reacts to failures and which response fields matter.
///
/// Design intent:
/// - pre_* hooks are BLOCKING: a failure denies the operation (fail-safe).
/// - post_* hooks are BEST-EFFORT: a failure silently skips.
/// - lifecycle/session hooks are advisory: failures are logged but never block.
/// - prompt/context/input surgery hooks are advisory by default so a slow/broken
///   plugin cannot freeze or corrupt the agent loop; callers may still apply
///   safe sub-modifies with their own validation.
pub fn hook_policy(hook_name: &str) -> HookPolicy {
    match hook_name {
        "pre_bash" | "pre_write" | "pre_read" | "pre_tool" | "pre_input" => HookPolicy {
            default_timeout_ms: DEFAULT_PRE_TIMEOUT_MS,
            fail_mode: HookFailMode::Deny,
            honor_allow: true,
            honor_modify: true,
        },
        "post_bash" | "post_write" | "post_read" | "post_tool" => HookPolicy {
            default_timeout_ms: DEFAULT_POST_TIMEOUT_MS,
            fail_mode: HookFailMode::Skip,
            honor_allow: false,
            honor_modify: true,
        },
        "session_start" | "session_stop" | "pre_compact" | "pre_turn" | "session_shutdown"
        | "pre_agent_start" | "pre_context" | "turn_start" | "turn_end" => HookPolicy {
            default_timeout_ms: DEFAULT_POST_TIMEOUT_MS,
            fail_mode: HookFailMode::Skip,
            honor_allow: false,
            honor_modify: true,
        },
        _ => HookPolicy {
            default_timeout_ms: DEFAULT_POST_TIMEOUT_MS,
            fail_mode: HookFailMode::Skip,
            honor_allow: false,
            honor_modify: true,
        },
    }
}

/// Default timeout in milliseconds for pre_* hooks (blocking — keep short).
pub const DEFAULT_PRE_TIMEOUT_MS: u64 = 5_000;

/// Default timeout in milliseconds for post_* and lifecycle hooks.
pub const DEFAULT_POST_TIMEOUT_MS: u64 = 30_000;
pub const MAX_PLUGIN_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_PLUGIN_OUTPUT_BYTES: usize = 1024 * 1024;
/// Maximum size of a prompt-backed command's template file (and its rendered
/// output). Keeps a misconfigured `prompt_file` from blowing up the agent
/// context. 256 KiB is generous for a research protocol yet bounded.
pub const MAX_PROMPT_COMMAND_BYTES: usize = 256 * 1024;

fn validate_plugin_io(label: &str, input_len: usize) -> Result<(), String> {
    if input_len > MAX_PLUGIN_INPUT_BYTES {
        Err(format!(
            "{label} input exceeds the {} byte limit",
            MAX_PLUGIN_INPUT_BYTES
        ))
    } else {
        Ok(())
    }
}

fn validate_plugin_output(label: &str, stdout: &[u8], stderr: &[u8]) -> Result<(), String> {
    if stdout.len() > MAX_PLUGIN_OUTPUT_BYTES || stderr.len() > MAX_PLUGIN_OUTPUT_BYTES {
        Err(format!(
            "{label} output exceeds the {} byte per-stream limit",
            MAX_PLUGIN_OUTPUT_BYTES
        ))
    } else {
        Ok(())
    }
}

async fn read_plugin_stream_bounded<R: AsyncRead + Unpin>(
    mut stream: Option<R>,
    label: &'static str,
) -> std::io::Result<Vec<u8>> {
    let Some(stream) = stream.as_mut() else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > MAX_PLUGIN_OUTPUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("plugin {label} exceeds the {MAX_PLUGIN_OUTPUT_BYTES} byte limit"),
            ));
        }
        output.extend_from_slice(&chunk[..read]);
    }
}

/// Wait for a plugin while draining stdout/stderr concurrently into bounded
/// buffers. `kill_on_drop(true)` on every caller's child guarantees timeout or
/// overflow tears down the process when the cancelled future drops it.
async fn bounded_plugin_output(
    mut child: tokio::process::Child,
    timeout: Duration,
) -> Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    tokio::time::timeout(timeout, async move {
        let (status, stdout, stderr) = tokio::try_join!(
            child.wait(),
            read_plugin_stream_bounded(stdout, "stdout"),
            read_plugin_stream_bounded(stderr, "stderr"),
        )?;
        Ok(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await
}

/// Normalized plugin/hook run result (host or microVM). `std::process::Output`
/// Run a plugin/hook/oauth/memory-provider script. When sandboxing is enabled
/// the script executes inside the microVM via the shared execution backend
/// (never directly on the host); the script directory is mounted read-only
/// (workspace plugins live under /workspace, global plugins under
/// /catcode-plugins). Host secrets are never inherited. The context payload
/// travels over stdin; stdout carries the response.
///
/// Returns the SAME shape as [`bounded_plugin_output`] so the existing
/// deny/skip/error match arms in the callers are unchanged.
async fn plugin_run(
    script: &std::path::Path,
    stdin: Vec<u8>,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> Result<std::io::Result<std::process::Output>, std::io::Error> {
    if crate::sandbox::is_sandbox_enabled() {
        return plugin_run_sandboxed(script, stdin, timeout, extra_env).await;
    }
    // Host path: spawn with the minimal env, write stdin, bound the wait.
    let mut child = match hook_command_with_env(script, extra_env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(std::io::Error::other(format!(
                "failed to spawn script {:?}: {e}",
                script
            ))))
        }
    };
    if !stdin.is_empty() {
        if let Some(mut child_stdin) = child.stdin.take() {
            let stdin_timeout = Duration::from_millis(timeout.as_millis().max(1000) as u64);
            let write_fut = async {
                use tokio::io::AsyncWriteExt;
                let _ = child_stdin.write_all(&stdin).await;
                let _ = child_stdin.shutdown().await;
            };
            if tokio::time::timeout(stdin_timeout, write_fut)
                .await
                .is_err()
            {
                let _ = child.start_kill();
                // Model a stdin timeout as an elapsed timeout so the caller's
                // existing `Err(_)` arm (deny/skip/error on timeout) handles it.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "plugin execution timed out",
                ));
            }
        }
    }
    match bounded_plugin_output(child, timeout).await {
        Ok(Ok(o)) => Ok(Ok(o)),
        Ok(Err(e)) => Ok(Err(e)),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "plugin execution timed out",
        )),
    }
}

/// Construct a `std::process::Output` from a guest exit code cross-platform.
fn output_from_exit(code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> std::process::Output {
    #[cfg(unix)]
    let status = {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    };
    #[cfg(not(unix))]
    let status = {
        // On non-Unix the only guest is Linux (sandboxed), so this branch is
        // unreachable in practice; fall back to a best-effort status.
        std::process::ExitStatus::default()
    };
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

/// Sandboxed plugin/hook run: translate the script path to its guest location,
/// pick the guest interpreter (Linux guest — no PowerShell), and exec via the
/// shared backend. Never falls back to the host on a backend error.
async fn plugin_run_sandboxed(
    script: &std::path::Path,
    stdin: Vec<u8>,
    timeout: Duration,
    extra_env: &[(String, String)],
) -> Result<std::io::Result<std::process::Output>, std::io::Error> {
    use crate::sandbox::policy;
    let cfg = crate::sandbox::config();
    let cfg = cfg.as_ref();
    // Translate the host script path to its guest mount point.
    let ws = cfg
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| cfg.workspace.clone());
    let plugin_dir = cfg
        .plugin_dir
        .canonicalize()
        .unwrap_or_else(|_| cfg.plugin_dir.clone());
    let script_canon = match script.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return Ok(Err(std::io::Error::other(format!(
                "script {:?} not found: {e}",
                script
            ))))
        }
    };
    let guest_path = if let Ok(rel) = script_canon.strip_prefix(&ws) {
        std::path::PathBuf::from("/workspace").join(rel)
    } else if plugin_dir.exists() {
        if let Ok(rel) = script_canon.strip_prefix(&plugin_dir) {
            std::path::PathBuf::from("/catcode-plugins").join(rel)
        } else {
            return Ok(Err(std::io::Error::other(format!(
                "script {:?} is not under the workspace or the global plugin dir; it cannot run in the sandbox",
                script
            ))));
        }
    } else {
        return Ok(Err(std::io::Error::other(format!(
            "script {:?} is not under the workspace and no global plugin dir is mounted",
            script
        ))));
    };
    let gp = guest_path.display().to_string();
    // Pick the guest interpreter from the extension (guest is Linux).
    let ext = script
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (program, args): (String, Vec<String>) = match ext.as_str() {
        "sh" | "bash" => ("bash".to_string(), vec![gp]),
        "py" => ("python3".to_string(), vec![gp]),
        "ps1" | "bat" | "cmd" | "exe" | "com" => {
            return Ok(Err(std::io::Error::other(format!(
                "script {:?} is a Windows-only type (.{ext}); the sandbox guest is Linux. \
                 Re-implement the plugin as a .sh or .py script.",
                script
            ))));
        }
        _ => ("sh".to_string(), vec![gp]),
    };
    let proc_env = policy::build_process_env(cfg, policy::ExecPurpose::Plugin);
    let mut env = proc_env.env;
    for (k, v) in extra_env {
        if !policy::is_secret_var(k) {
            env.insert(k.clone(), v.clone());
        }
    }
    let cwd = policy::effective_cwd(cfg, "").unwrap_or_else(|_| cfg.workspace.clone());
    let req = crate::sandbox::ExecRequest {
        program,
        args,
        cwd,
        env,
        inherit_parent_env: false,
        stdin: Some(stdin),
        timeout,
        ..Default::default()
    };
    match crate::sandbox::execution_backend().execute(req).await {
        Ok(r) => {
            let code = r.exit_code.unwrap_or(127);
            Ok(Ok(output_from_exit(code, r.stdout, r.stderr)))
        }
        Err(crate::sandbox::ExecutionError::Timeout(_)) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "plugin execution timed out",
        )),
        Err(e) => Ok(Err(std::io::Error::other(e.user_message()))),
    }
}

/// Slash-command names reserved by the harness. Plugin commands may not reuse
/// these (with or without a leading `/`).
const RESERVED_COMMAND_NAMES: &[&str] = &[
    "help",
    "new",
    "abort",
    "login",
    "logout",
    "models",
    "plugin-install",
    "plugin-list",
    "plugin-enable",
    "plugin-disable",
    "plugin-remove",
    "plugin-reload",
    "plugin-config",
    "stats",
    "sessions",
    "clear",
    "reset",
    "undo",
    "compact",
    "memory",
    "remember",
    "forget",
    "reflect",
    "index",
    "goal",
    "cancel-goal",
    "run",
    "parallel",
    "chain",
    "subagents",
    "vision",
    "context",
    "usage",
    "skill",
];

// ---- manifest deserialization (plugin.json) ----

#[derive(Deserialize, Debug, Clone)]
struct PluginManifest {
    name: String,
    version: String,
    #[serde(default)]
    protocol_version: Option<u32>,
    /// Explicit authority requested by newer plugins. Absent keeps legacy
    /// manifests compatible by inferring the minimum set from their features.
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    hooks: HashMap<String, HookManifestEntry>,
    /// Optional user-declared tools (custom capabilities, no MCP needed).
    #[serde(default)]
    tools: Vec<ToolManifestEntry>,
    /// Built-in/plugin tool names to REMOVE from the model's toolset.
    #[serde(default)]
    disable_tools: Vec<String>,
    /// Static text injected into the system prompt (empty = none).
    #[serde(default)]
    system_prompt: String,
    /// Optional OAuth subscription provider this plugin adds (login flow +
    /// token resolution), mirroring the built-in OpenAI/Claude/Gemini OAuth.
    #[serde(default)]
    oauth: Option<OauthManifestEntry>,
    /// Optional memory backend that replaces the built-in markdown store for
    /// standing-prompt injection, slash memory commands, compaction extracts,
    /// and (when no tool overrides `memory`) the built-in `memory` tool.
    #[serde(default)]
    memory_provider: Option<MemoryProviderManifestEntry>,
    /// Optional slash commands declared by the plugin (`/name` → script).
    #[serde(default)]
    commands: Vec<CommandManifestEntry>,
}

/// A slash-command declared in a plugin manifest (the `commands` array).
///
/// A command is either **script-backed** (`script` → run an executable, show
/// its output) or **prompt-backed** (`prompt_file` → render a template and
/// submit it as a normal agent turn). Exactly one of `script`/`prompt_file`
/// must be set. `mode` is optional and inferred when absent.
#[derive(Deserialize, Debug, Clone)]
struct CommandManifestEntry {
    name: String,
    #[serde(default)]
    description: String,
    /// Executable handler script (script-backed commands). Mutually exclusive
    /// with `prompt_file`.
    #[serde(default)]
    script: Option<String>,
    /// Prompt template file (prompt-backed commands) rendered and submitted as
    /// an agent turn. Path-confined to the plugin directory (no absolute
    /// paths, no `..`). Mutually exclusive with `script`.
    #[serde(default)]
    prompt_file: Option<String>,
    /// Optional explicit mode: `"script"` or `"agent_turn"`. Inferred from
    /// which of `script`/`prompt_file` is set when absent.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// A memory-provider declaration in `plugin.json` (`memory_provider` block).
#[derive(Deserialize, Debug, Clone)]
struct MemoryProviderManifestEntry {
    script: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
struct HookManifestEntry {
    script: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    pass_args: bool,
}

/// A tool declared in a plugin manifest (the `tools` array). Each entry becomes
/// a tool the model can call; the `script` handler is spawned per call.
#[derive(Deserialize, Debug, Clone)]
struct ToolManifestEntry {
    name: String,
    #[serde(default)]
    description: String,
    /// JSON Schema for the tool's parameters (sent to the model as-is).
    #[serde(default)]
    parameters: Value,
    script: String,
    /// "readonly" (skip the approval gate) or "destructive" (prompt; default).
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// When true AND `name` matches a built-in tool, this plugin's handler
    /// REPLACES the built-in's implementation: the model still sees a tool of
    /// that name (the plugin's declared schema), but calls route to the plugin
    /// script instead of the core handler. Lets a plugin fully override a
    /// core tool (a sandboxed bash, a redacting read_file, …) without
    /// recompiling. Default false: a name collision stays built-in (unchanged).
    #[serde(default, rename = "override")]
    override_builtin: bool,
}

/// An OAuth provider declared by a plugin manifest's `oauth` block. Lets a
/// plugin add a subscription-OAuth provider (login flow + token resolution)
/// the same way the built-in OpenAI/Claude/Gemini providers work — no
/// recompile. The plugin supplies ONE script that handles four actions
/// (`login`, `complete`, `token`, `clear`) dispatched by an `action` field in
/// the stdin context; per-action script overrides are optional. See the `plugin-authoring` skill
/// (`.catalyst-code/skills/plugin-authoring/SKILL.md`) for the full contract.
#[derive(Deserialize, Debug, Clone)]
struct OauthManifestEntry {
    /// The provider identity. Must match the provider-config `name` created on
    /// `/login` (the harness creates the config with this name). Also the key
    /// `/oauth-code` and `/logout` dispatch on.
    provider_id: String,
    /// Human label shown in the `/login` picker (defaults to provider_id).
    #[serde(default)]
    label: Option<String>,
    /// Wire protocol: "openai" (default) or "anthropic".
    #[serde(default)]
    kind: Option<String>,
    /// The endpoint base URL (include `/v1`; paths appended directly).
    base_url: String,
    #[serde(default)]
    description: Option<String>,
    /// Extra HTTP headers appended to every request, `[[key,val],…]`.
    #[serde(default)]
    headers: Vec<(String, String)>,
    /// Token-file name, relative to `~/.config/catalyst-code/oauth/`. Defaults
    /// to `<provider_id>.json`. The harness passes the ABSOLUTE resolved path to
    /// every script invocation, so the plugin owns the token's on-disk format.
    #[serde(default)]
    token_path: Option<String>,
    /// Optional external credential file to detect and import. The supported
    /// `$CODEX_HOME/auth.json` form follows the official Codex CLI store; the
    /// harness uses it only as a cheap credential-presence hint and leaves the
    /// actual format/import logic to the provider script.
    #[serde(default)]
    detect_path: Option<String>,
    /// The script handling ALL actions (login/complete/token/clear). Required
    /// unless every action has an explicit override.
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    login_script: Option<String>,
    #[serde(default)]
    complete_script: Option<String>,
    #[serde(default)]
    token_script: Option<String>,
    /// Timeout for the login + complete actions (default 120s).
    #[serde(default)]
    login_timeout_ms: Option<u64>,
    /// Timeout for the token (resolve/refresh) action (default 30s).
    #[serde(default)]
    token_timeout_ms: Option<u64>,
    /// Optional path for the loopback redirect (e.g. ``"/oauth2callback"``
    /// for Google's installed-app OAuth clients). Defaults to ``"/callback"``
    /// which is what most plugins use; override when the OAuth provider's
    /// registered redirect URI has a different path.
    #[serde(default)]
    redirect_path: Option<String>,
    /// Non-secret env var names the harness forwards to this provider's
    /// scripts (e.g. `ACME_OAUTH_HOST` for a self-hosted auth server). The
    /// harness otherwise scrubs the environment, so plugin-specific config
    /// knobs must be declared here. Names containing KEY/TOKEN/SECRET/
    /// PASSWORD are rejected — passthrough must never defeat env scrubbing.
    #[serde(default)]
    env_passthrough: Vec<String>,
}

// ---- public types ----

/// A loaded plugin with its registered hooks and declared tools.
#[derive(Clone, Debug)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub protocol_version: u32,
    pub capabilities: Vec<String>,
    pub description: String,
    pub enabled: bool,
    /// Absolute path to the plugin directory on disk.
    pub source_path: PathBuf,
    /// Hook name → config map.
    pub hooks: HashMap<String, HookConfig>,
    /// Tools this plugin declares (custom capabilities; no MCP needed).
    pub tools: Vec<ToolConfig>,
    /// Slash commands this plugin declares (`/name` handlers).
    pub commands: Vec<CommandConfig>,
    /// Built-in/plugin tool names to REMOVE from the model's toolset.
    pub disable_tools: Vec<String>,
    /// Static text injected into the system prompt (empty = none).
    pub system_prompt: String,
    /// OAuth subscription provider this plugin declares, if any.
    pub oauth: Option<PluginOauthConfig>,
    /// Memory backend that replaces the built-in markdown store, if any.
    pub memory_provider: Option<PluginMemoryProviderConfig>,
}

/// A loaded memory-provider declaration (`memory_provider` block with the
/// script resolved to an absolute, path-confined, executable file).
#[derive(Clone, Debug)]
pub struct PluginMemoryProviderConfig {
    /// Plugin that owns this provider (for logging / framing).
    pub plugin_name: String,
    /// Absolute path to the provider script.
    pub script: PathBuf,
    /// Hard timeout in milliseconds per action.
    pub timeout_ms: u64,
}

/// Configuration for one hook within a plugin.
#[derive(Clone, Debug)]
pub struct HookConfig {
    /// Absolute path to the executable hook script.
    pub script: PathBuf,
    /// Hard timeout in milliseconds for this hook.
    pub timeout_ms: u64,
    /// Whether to include tool args in the hook context JSON.
    pub pass_args: bool,
}

/// Configuration for one user-declared tool within a plugin.
#[derive(Clone, Debug)]
pub struct ToolConfig {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's parameters (sent to the model verbatim).
    pub parameters: Value,
    /// Absolute path to the executable handler script.
    pub script: PathBuf,
    /// Hard timeout in milliseconds for a single tool call.
    pub timeout_ms: u64,
    /// Approval classification: ReadOnly skips the gate, Destructive prompts.
    pub kind: ToolKind,
    /// True → this tool's handler replaces the built-in of the same name.
    pub override_builtin: bool,
    /// Plugin that owns this tool (for UI side-effect framing).
    pub plugin_name: String,
}

/// How a plugin slash command is executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandMode {
    /// Run an executable handler script; its `output` is shown to the user.
    Script,
    /// Render a prompt template and submit it as a normal agent turn (the same
    /// path as a user `send`). Lets a command like `/deep-research` drive a
    /// full agent loop without a script or recompile.
    AgentTurn,
}

impl CommandMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CommandMode::Script => "script",
            CommandMode::AgentTurn => "agent_turn",
        }
    }
}

/// Configuration for one plugin-declared slash command.
#[derive(Clone, Debug)]
pub struct CommandConfig {
    pub name: String,
    pub description: String,
    /// How the command is executed (script vs. agent-turn prompt).
    pub mode: CommandMode,
    /// Absolute path to the executable handler script (script-backed commands).
    pub script: Option<PathBuf>,
    /// Absolute path to the prompt template file (prompt-backed commands).
    pub prompt_file: Option<PathBuf>,
    /// Hard timeout in milliseconds for a single command invocation.
    pub timeout_ms: u64,
    /// Plugin that owns this command.
    pub plugin_name: String,
}

/// A loaded OAuth-provider declaration (manifest `oauth` block with script
/// paths resolved to absolute, path-confined, executable files). The plugin
/// owns the token's on-disk format; the harness owns the loopback redirect
/// server (web flow), optional automatic completion, and the `/oauth-code`
/// paste path (manual flow).
#[derive(Clone, Debug)]
pub struct PluginOauthConfig {
    pub provider_id: String,
    pub label: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub description: String,
    pub headers: Vec<(String, String)>,
    /// Loopback redirect path. Default ``"/callback"``. Override when the
    /// provider's registered redirect URI uses a different path (e.g. Google's
    /// installed-app OAuth clients expect ``"/oauth2callback"``).
    pub redirect_path: String,
    /// Absolute path the plugin reads/writes its token at.
    pub token_path: PathBuf,
    /// Optional external credential path used for cheap login detection.
    pub detect_path: Option<PathBuf>,
    /// Resolved absolute script paths per action (override else the default).
    pub scripts: HashMap<String, PathBuf>,
    pub login_timeout_ms: u64,
    pub token_timeout_ms: u64,
    /// Non-secret env var names forwarded to this provider's scripts
    /// (validated in `load_oauth_entry`; values taken from the harness env).
    pub env_passthrough: Vec<String>,
}

impl PluginOauthConfig {
    /// Resolve which script runs `action` (the action-specific override, else
    /// the shared `script` fallback).
    pub fn script_for(&self, action: &str) -> Option<&Path> {
        self.scripts
            .get(action)
            .or_else(|| self.scripts.get("*"))
            .map(|p| p.as_path())
    }
}

/// A cached OAuth access token + optional request headers + absolute-seconds
/// expiry, keyed by provider_id in the PluginManager. Keeps the per-turn hot
/// path (enrich_oauth) from spawning the token script on every request.
/// `headers` come from the plugin `token` action (e.g. `chatgpt-account-id`).
#[derive(Clone)]
struct CachedToken {
    token: String,
    expires_at: u64,
    headers: Vec<(String, String)>,
}

/// Fresh OAuth credentials resolved from a plugin `token` action (or cache).
#[derive(Clone, Debug)]
pub struct ResolvedOauthCreds {
    pub access_token: String,
    /// Extra HTTP headers to merge onto the resolved provider for this turn
    /// (e.g. `chatgpt-account-id`). Empty when the script omits them.
    pub headers: Vec<(String, String)>,
}

/// Result returned from executing a hook.
#[derive(Clone, Debug)]
pub struct HookResult {
    /// Whether the operation is allowed to proceed.
    pub allow: bool,
    /// Human-readable explanation from the hook.
    pub reason: String,
    /// Optional modified arguments (pre hooks only; ignored for post hooks).
    pub modify: Option<Value>,
    /// Optional UI notification text from the hook response.
    pub notify: Option<String>,
    /// Optional status-bar text (`Some("")` means clear).
    pub status: Option<String>,
}

pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;
const PLUGIN_CAPABILITIES: &[&str] = &[
    "read_workspace",
    "write_workspace",
    "execute_subprocess",
    "access_network",
    "receive_prompts",
    "receive_tool_arguments",
    "receive_model_responses",
    "access_secrets",
    "register_tools",
    "register_commands",
    "register_providers",
    "register_memory_backend",
];

fn validate_manifest_capabilities(manifest: &PluginManifest) -> Result<Vec<String>, String> {
    let protocol_version = manifest.protocol_version.unwrap_or(1);
    if protocol_version > PLUGIN_PROTOCOL_VERSION {
        return Err(format!(
            "plugin protocol version {protocol_version} is newer than supported ({PLUGIN_PROTOCOL_VERSION})"
        ));
    }

    let mut required = HashSet::<&str>::new();
    let uses_subprocess = !manifest.hooks.is_empty()
        || !manifest.tools.is_empty()
        || manifest.commands.iter().any(|c| {
            c.script
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
        })
        || manifest.oauth.is_some()
        || manifest.memory_provider.is_some();
    if uses_subprocess {
        required.insert("execute_subprocess");
    }
    for (name, hook) in &manifest.hooks {
        if hook.pass_args {
            required.insert("receive_tool_arguments");
        }
        if matches!(
            name.as_str(),
            "pre_input" | "pre_context" | "pre_turn" | "turn_start"
        ) {
            required.insert("receive_prompts");
        }
        if matches!(name.as_str(), "post_tool" | "turn_end" | "session_stop") {
            required.insert("receive_model_responses");
        }
    }
    if !manifest.tools.is_empty() {
        required.insert("register_tools");
        for tool in &manifest.tools {
            if tool.kind.as_deref() == Some("readonly") {
                required.insert("read_workspace");
            } else {
                required.insert("write_workspace");
            }
        }
    }
    if !manifest.commands.is_empty() {
        required.insert("register_commands");
    }
    if manifest.oauth.is_some() {
        required.extend(["register_providers", "access_network", "access_secrets"]);
    }
    if manifest.memory_provider.is_some() {
        required.insert("register_memory_backend");
    }

    let Some(declared) = manifest.capabilities.as_ref() else {
        let mut inferred: Vec<String> = required.into_iter().map(str::to_string).collect();
        inferred.sort();
        return Ok(inferred);
    };
    let declared_set: HashSet<&str> = declared.iter().map(String::as_str).collect();
    let unknown: Vec<&str> = declared_set
        .iter()
        .copied()
        .filter(|capability| !PLUGIN_CAPABILITIES.contains(capability))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "plugin declares unknown capabilities: {}",
            unknown.join(", ")
        ));
    }
    let mut missing: Vec<&str> = required.difference(&declared_set).copied().collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        return Err(format!(
            "plugin is denied because its manifest is missing required capabilities: {}",
            missing.join(", ")
        ));
    }
    let mut validated = declared.clone();
    validated.sort();
    validated.dedup();
    Ok(validated)
}

// ---- PluginManager ----

/// Where `/plugin-install` copies a plugin on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginInstallScope {
    /// `~/.catalyst-code/plugins` — available in every workspace.
    Global,
    /// `<workspace>/.catalyst-code/plugins` — this repo only.
    Workspace,
}

impl PluginInstallScope {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "" | "global" | "user" | "g" | "-g" | "--global" => Ok(Self::Global),
            "workspace" | "project" | "local" | "w" | "-w" | "--workspace" => Ok(Self::Workspace),
            other => Err(format!(
                "unknown plugin scope '{other}' (use 'global' or 'workspace')"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

/// A project-scoped plugin that was NOT loaded because it is not trusted
/// (or its trust decision is still pending). Carries enough manifest metadata
/// for the trust prompt UI plus the recorded decision.
#[derive(Clone, Debug)]
pub struct SkippedProjectPlugin {
    pub name: String,
    pub version: String,
    pub description: String,
    pub path: PathBuf,
    /// Stable key for the project plugin directory, independent of the
    /// manifest's mutable display name.
    pub decision_key: String,
    /// Recorded user decision: `Some("trust")` → would load; `Some("deny")`
    /// → stays skipped; `None` → undecided (drives the auto trust prompt).
    pub decision: Option<String>,
}

/// Manages the lifecycle of all installed plugins.
/// Holds an in-memory registry behind a `RwLock`.
pub struct PluginManager {
    plugins_dir: PathBuf,
    /// Optional **global, user-owned** plugins dir (`~/.catalyst-code/plugins`)
    /// scanned before the project dir so globally-staged plugins load across
    /// every project. `None` for the isolated `new()` constructor (used by
    /// tests); `Some` for `new_with_global_plugins()` (used by the core at
    /// startup). A project plugin with the same name overrides the global one.
    user_plugins_dir: Option<PathBuf>,
    /// Workspace root — used to decide whether a plugin dir is project-scoped
    /// (inside the workspace) vs user-installed (outside it).
    workspace: PathBuf,
    /// When false (the secure default), project-scoped plugins under the
    /// workspace's `.catalyst-code/plugins` are NOT auto-loaded — a repo you
    /// `cd` into must not run hook scripts with your privileges without opt-in.
    trust_project: bool,
    /// Canonical workspace root used as the trust-store key.
    trust_key: PathBuf,
    /// Per-project trust decisions (canonical plugin directory key →
    /// "trust" | "deny") loaded from the user-owned store; consulted when
    /// gating project plugins.
    trust_decisions: RwLock<HashMap<String, String>>,
    /// Where decisions are persisted (`~/.config/catalyst-code/plugin-trust.json`);
    /// None for isolated test constructors so tests never touch the real store.
    trust_store_path: Option<PathBuf>,
    plugins: RwLock<HashMap<String, Plugin>>,
    /// Project-scoped plugins skipped because they are not trusted (or the
    /// trust decision is still pending). Carries manifest metadata + decision
    /// so the UI can render the trust prompt.
    skipped_project: Mutex<Vec<SkippedProjectPlugin>>,
    /// In-memory cache of resolved OAuth access tokens (provider_id → token),
    /// so the per-turn hot path (`enrich_oauth`) doesn't spawn the token script
    /// on every request. Refreshed when near expiry.
    token_cache: Mutex<HashMap<String, CachedToken>>,
}

impl PluginManager {
    /// Resolve a possibly-relative project plugins dir against `workspace`.
    /// Keeps install + scan on the same absolute path regardless of process cwd
    /// (the TUI passes `--workspace .` but cwd can still drift across restarts).
    fn resolve_plugins_dir(plugins_dir: PathBuf, workspace: &Path) -> PathBuf {
        if plugins_dir.is_absolute() {
            plugins_dir
        } else {
            workspace.join(plugins_dir)
        }
    }
}

/// Canonical (symlink-resolved) form of `workspace`, used as the trust-store
/// key so the same project always maps to the same decisions regardless of how
/// it was opened.
fn canonical_workspace(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf())
}

/// Stable trust-store key for one project plugin directory. Decisions are
/// intentionally bound to the directory, not only the mutable manifest name.
fn canonical_plugin_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

impl PluginManager {
    /// Create a new manager and scan/load all plugins from `plugins_dir` only
    /// (the project plugins dir). This is the **isolated** constructor used by
    /// tests; it does NOT scan the global `~/.catalyst-code/plugins` dir, so
    /// tests are unaffected by the developer's real global plugins.
    ///
    /// Production code should use [`PluginManager::new_with_global_plugins`]
    /// instead, which also loads globally-staged plugins.
    pub fn new(plugins_dir: PathBuf, workspace: PathBuf, trust_project: bool) -> Self {
        let plugins_dir = Self::resolve_plugins_dir(plugins_dir, &workspace);
        let trust_key = canonical_workspace(&workspace);
        let mgr = PluginManager {
            plugins_dir,
            user_plugins_dir: None,
            workspace,
            trust_project,
            trust_key,
            trust_decisions: RwLock::new(HashMap::new()),
            trust_store_path: None,
            plugins: RwLock::new(HashMap::new()),
            skipped_project: Mutex::new(Vec::new()),
            token_cache: Mutex::new(HashMap::new()),
        };
        mgr.scan_and_load();
        mgr
    }

    /// Production constructor: like [`PluginManager::new`] but ALSO scans the
    /// global, user-owned plugins dir (`~/.catalyst-code/plugins`, staged on
    /// first run) before the project dir. Globally-staged plugins (e.g. the
    /// vision-handoff plugin) therefore load in every project without any
    /// per-project setup; a same-named project plugin overrides the global one.
    pub fn new_with_global_plugins(
        plugins_dir: PathBuf,
        workspace: PathBuf,
        trust_project: bool,
    ) -> Self {
        let plugins_dir = Self::resolve_plugins_dir(plugins_dir, &workspace);
        let user_plugins_dir = crate::config::home_dir().map(|h| h.join(".catalyst-code/plugins"));
        let trust_store_path = crate::plugin_trust::plugin_trust_store_path();
        let trust_decisions = trust_store_path
            .as_ref()
            .map(|p| crate::plugin_trust::PluginTrustStore::load(p).decisions_for(&workspace))
            .unwrap_or_default();
        let trust_key = canonical_workspace(&workspace);
        let mgr = PluginManager {
            plugins_dir,
            user_plugins_dir,
            workspace,
            trust_project,
            trust_key,
            trust_decisions: RwLock::new(trust_decisions),
            trust_store_path,
            plugins: RwLock::new(HashMap::new()),
            skipped_project: Mutex::new(Vec::new()),
            token_cache: Mutex::new(HashMap::new()),
        };
        mgr.scan_and_load();
        mgr
    }

    /// Test-only constructor that scans an explicit user (global) plugins dir
    /// in addition to the project dir, so global-scan behavior can be exercised
    /// deterministically without touching the real `~/.catalyst-code/plugins`.
    #[cfg(test)]
    fn new_with_user_plugins_dir(
        plugins_dir: PathBuf,
        user_plugins_dir: Option<PathBuf>,
        workspace: PathBuf,
        trust_project: bool,
    ) -> Self {
        let plugins_dir = Self::resolve_plugins_dir(plugins_dir, &workspace);
        let trust_key = canonical_workspace(&workspace);
        let mgr = PluginManager {
            plugins_dir,
            user_plugins_dir,
            workspace,
            trust_project,
            trust_key,
            trust_decisions: RwLock::new(HashMap::new()),
            trust_store_path: None,
            plugins: RwLock::new(HashMap::new()),
            skipped_project: Mutex::new(Vec::new()),
            token_cache: Mutex::new(HashMap::new()),
        };
        mgr.scan_and_load();
        mgr
    }

    /// Names of project-scoped plugins skipped because they are not trusted.
    /// The caller surfaces these to the user so the opt-in is discoverable.
    pub fn skipped_project_plugins(&self) -> Vec<String> {
        self.skipped_project
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.name.clone())
            .collect()
    }

    /// Project-scoped plugins that were skipped with NO recorded decision yet.
    /// Non-empty drives the auto trust prompt on startup.
    pub fn pending_trust_plugins(&self) -> Vec<SkippedProjectPlugin> {
        self.skipped_project
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.decision.is_none())
            .cloned()
            .collect()
    }

    /// All skipped project-scoped plugins with their current decision (`` for
    /// undecided) — the full list for the trust prompt (`/plugin-trust`).
    pub fn trust_prompt_plugins(&self) -> Vec<SkippedProjectPlugin> {
        let mut entries = self.skipped_project.lock().unwrap().clone();
        let known: std::collections::HashSet<String> =
            entries.iter().map(|p| p.decision_key.clone()).collect();
        let decisions = self.trust_decisions.read().unwrap();
        for plugin in self.plugins.read().unwrap().values() {
            if self.scope_of_path(&plugin.source_path) != PluginInstallScope::Workspace {
                continue;
            }
            let decision_key = canonical_plugin_key(&plugin.source_path);
            if known.contains(&decision_key) {
                continue;
            }
            entries.push(SkippedProjectPlugin {
                name: plugin.name.clone(),
                version: plugin.version.clone(),
                description: plugin.description.clone(),
                path: plugin.source_path.clone(),
                decision_key: decision_key.clone(),
                decision: decisions.get(&decision_key).cloned(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
        entries
    }

    /// JSON entries for the `plugin_trust_prompt` event payload: one per
    /// skipped project-scoped plugin, with manifest metadata + the recorded
    /// decision ("" = undecided).
    pub fn trust_prompt_json(&self) -> Vec<Value> {
        self.trust_prompt_plugins()
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "version": p.version,
                    "description": p.description,
                    "path": p.path.display().to_string(),
                    "decision": p.decision.clone().unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Record the user's trust decisions for this project's plugins and
    /// re-scan so newly-trusted plugins load immediately. Decisions are
    /// persisted to the user-owned store (production) so the auto prompt is
    /// not re-shown on later loads. Returns Err on persistence failure;
    /// validation of values is the caller's job.
    pub fn apply_trust_decisions(
        &self,
        decisions: HashMap<String, String>,
    ) -> Result<Value, String> {
        let known = self.trust_prompt_plugins();
        let keyed_decisions: HashMap<String, String> = decisions
            .iter()
            .filter_map(|(name_or_path, value)| {
                let key = known
                    .iter()
                    .find(|p| &p.name == name_or_path || &p.decision_key == name_or_path)
                    .map(|p| p.decision_key.clone())
                    .or_else(|| {
                        self.find_project_plugin_dir(name_or_path)
                            .map(|p| canonical_plugin_key(&p))
                    })?;
                Some((key, value.clone()))
            })
            .collect();
        {
            let mut cur = self.trust_decisions.write().unwrap();
            // Merge: only the entries in this partial update change; prior
            // decisions for other plugin directories are preserved.
            for (key, value) in &keyed_decisions {
                cur.insert(key.clone(), value.clone());
            }
        }
        // Persist best-effort: a failed write keeps decisions in memory for
        // this run but reports the error so the UI can surface it.
        if let Some(ref path) = self.trust_store_path {
            // Decisions are a shared read-modify-write store: serialize the
            // full load/merge/save transaction so two cores cannot lose each
            // other's per-plugin updates.
            let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
                .map_err(|e| format!("failed to lock plugin trust store: {e}"))?;
            let mut store = crate::plugin_trust::PluginTrustStore::load(path);
            let mut stored = store.decisions_for(&self.trust_key);
            for (key, value) in &keyed_decisions {
                stored.insert(key.clone(), value.clone());
            }
            store.set_decisions(&self.trust_key, &stored);
            store.save(path)?;
        }
        // Reload (preserves enabled/disabled flags + OAuth token cache) so
        // newly-trusted plugins load immediately.
        let summary = self.reload();
        if let Some(errors) = summary.get("errors").and_then(|v| v.as_array()) {
            for e in errors {
                eprintln!("[plugins] {e}");
            }
        }
        Ok(summary)
    }

    fn find_project_plugin_dir(&self, name: &str) -> Option<PathBuf> {
        let candidate = self.plugins_dir.join(name);
        candidate.join("plugin.json").is_file().then_some(candidate)
    }

    fn user_installed_keys(&self) -> Vec<String> {
        let Some(path) = &self.trust_store_path else {
            return Vec::new();
        };
        let store = crate::plugin_trust::PluginTrustStore::load(path);
        store
            .installed_plugins_for(&self.trust_key)
            .into_iter()
            .collect()
    }

    /// Re-scan the plugin directories and load/reload all valid plugins.
    ///
    /// Two directories are scanned:
    /// 1. the **global, user-owned** plugins dir `~/.catalyst-code/plugins`
    ///    (staged on first run; shared across every project; loads always —
    ///    these are plugins *you* installed, outside the workspace), then
    /// 2. the **project** plugins dir (`plugins_dir`, default
    ///    `.catalyst-code/plugins` inside the workspace; gated by
    ///    `trust_project`).
    ///
    /// On a name collision the project plugin wins, so a project's own
    /// `.catalyst-code/plugins/<name>` overrides the global one for that
    /// project only — matching the agent/skill override model.
    ///
    /// Invalid plugins are skipped with a log message to stderr but never
    /// crash. Project-scoped plugins (dir inside the workspace) are skipped
    /// unless `trust_project` is true; their names are recorded in
    /// `skipped_project`. Returns load-error messages for callers (e.g. reload).
    fn scan_and_load(&self) -> Vec<String> {
        let canon_ws =
            std::fs::canonicalize(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
        let mut plugins = self.plugins.write().unwrap();
        plugins.clear();
        let mut skipped_local: Vec<SkippedProjectPlugin> = Vec::new();
        let mut errors: Vec<String> = Vec::new();

        // 1) Global, user-owned plugins (~/.catalyst-code/plugins) when this
        //    manager was constructed to scan them (production). The explicit
        //    `user_owned` flag is authoritative even if HOME happens to be
        //    nested under the workspace; these are not project plugins.
        //    Skipped entirely for the isolated `new()` constructor (tests).
        if let Some(ref user_dir) = self.user_plugins_dir {
            self.scan_dir(
                user_dir,
                &canon_ws,
                true,
                &mut plugins,
                &mut skipped_local,
                &mut errors,
            );
        }

        // 2) Project plugins. Scanned last so a same-named project plugin
        //    overrides the global one. Created on demand so the dir existing
        //    never errors.
        let _ = std::fs::create_dir_all(&self.plugins_dir);
        self.scan_dir(
            &self.plugins_dir,
            &canon_ws,
            false,
            &mut plugins,
            &mut skipped_local,
            &mut errors,
        );

        *self.skipped_project.lock().unwrap() = skipped_local;
        errors
    }

    /// Scan one plugin directory and load every valid plugin in it into
    /// `plugins` (later inserts override earlier ones on name collision).
    /// `user_owned` identifies the explicit global/user directory. It is kept
    /// separate from the filesystem-prefix check so a user-owned directory
    /// nested inside a workspace is not mistaken for a repo plugin.
    /// `skipped` collects project-scoped plugins gated off by trust (denied or
    /// undecided). A missing or unreadable directory is a silent no-op.
    fn scan_dir(
        &self,
        dir: &std::path::Path,
        canon_ws: &std::path::Path,
        user_owned: bool,
        plugins: &mut HashMap<String, Plugin>,
        skipped: &mut Vec<SkippedProjectPlugin>,
        errors: &mut Vec<String>,
    ) {
        let rd = match std::fs::read_dir(dir) {
            Ok(r) => r,
            Err(_) => return,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest_path = path.join("plugin.json");
            if !manifest_path.exists() {
                continue;
            }
            // Project-scoped gating: a plugin dir inside the workspace (e.g.
            // `.catalyst-code/plugins/*` shipped by the repo) is treated as
            // untrusted unless the user opted in (via `trust_project`, a
            // user-install marker, or a recorded trust decision). This stops a
            // repo from auto-running hook scripts (which see every tool's
            // args, including bash commands + file contents) with your
            // privileges. User-owned plugins from the explicit global scan
            // load regardless, even if their path is nested. `trust_project`
            // is read only from env/CLI, never a project config file, so a repo
            // can't self-enable.
            let canon_plugin = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            // Classify project scope from the configured directory's lexical
            // location, not only its resolved target. A repo can ship a
            // symlinked plugin directory that points outside the workspace;
            // that must remain gated as repo code, not become trusted merely
            // because its target happens to be elsewhere.
            let physically_in_project = canon_plugin.starts_with(canon_ws);
            // Decide whether this scan root is project-scoped from its lexical
            // path before resolving child symlinks. A project plugin symlinked
            // to an external directory must remain gated; a custom plugin dir
            // outside the workspace remains user-owned by location.
            let workspace_abs =
                std::path::absolute(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
            let scan_dir_abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
            let is_project = !user_owned && scan_dir_abs.starts_with(&workspace_abs);
            // A repo symlink must not inherit the exemption from a marker in
            // an external user-installed plugin it happens to target.
            let user_installed = physically_in_project
                && (self
                    .user_installed_keys()
                    .contains(&canonical_plugin_key(&path))
                    || (cfg!(test) && path.join(".catalyst-plugin-user-installed").is_file()));
            let mut skip: Option<SkippedProjectPlugin> = None;
            if !user_owned && is_project && !self.trust_project && !user_installed {
                let manifest: Value = std::fs::read_to_string(&manifest_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_else(|| Value::Null);
                let name = manifest
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        path.file_name()
                            .and_then(|n| n.to_str())
                            .map(String::from)
                            .unwrap_or_default()
                    });
                // A recorded "trust" decision loads the plugin; anything else
                // (deny or undecided) keeps it gated off and records it so the
                // trust prompt can list it.
                let decision_key = canonical_plugin_key(&path);
                let decision = self
                    .trust_decisions
                    .read()
                    .unwrap()
                    .get(&decision_key)
                    .cloned();
                if decision.as_deref() != Some("trust") {
                    let version = manifest
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let description = manifest
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string();
                    eprintln!(
                        "[plugins] skipping project-scoped plugin '{name}' (in {canon_plugin:?}); trust it with the plugin trust prompt (/plugin-trust), set --trust-project-plugins / CATALYST_CODE_TRUST_PROJECT_PLUGINS=1, or reinstall with `/plugin-install … workspace`"
                    );
                    skip = Some(SkippedProjectPlugin {
                        name,
                        version,
                        description,
                        path: canon_plugin,
                        decision_key,
                        decision,
                    });
                }
            }
            if let Some(s) = skip {
                skipped.push(s);
                continue;
            }
            match Self::load_plugin_from_dir(&path) {
                Ok(plugin) => {
                    plugins.insert(plugin.name.clone(), plugin);
                }
                Err(e) => {
                    let msg = format!(
                        "failed to load plugin in {:?}: {e}",
                        path.file_name().unwrap_or_default()
                    );
                    eprintln!("[plugins] {msg}");
                    errors.push(msg);
                }
            }
        }
    }

    fn validate_plugin_name(name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("plugin name is empty".into());
        }
        if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
            return Err(format!("plugin name contains a path separator: {name:?}"));
        }
        if Path::new(name).components().count() != 1 {
            return Err(format!(
                "plugin name must be a single directory name: {name:?}"
            ));
        }
        Ok(())
    }

    /// Load a single plugin from a directory containing plugin.json.
    fn load_plugin_from_dir(dir: &Path) -> Result<Plugin, String> {
        let manifest_path = dir.join("plugin.json");
        let raw = std::fs::read_to_string(&manifest_path)
            .map_err(|e| format!("cannot read plugin.json: {e}"))?;

        let manifest: PluginManifest =
            serde_json::from_str(&raw).map_err(|e| format!("plugin.json parse error: {e}"))?;

        Self::validate_plugin_name(&manifest.name)?;
        let capabilities = validate_manifest_capabilities(&manifest)?;
        let plugin_protocol_version = manifest.protocol_version.unwrap_or(1);

        // Canonicalize the plugin directory for path-confinement checks.
        let canon_dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());

        let mut hooks: HashMap<String, HookConfig> = HashMap::new();
        for (hook_name, entry) in &manifest.hooks {
            if !HOOK_POINTS.contains(&hook_name.as_str()) {
                eprintln!(
                    "[plugins] unknown hook point '{}' in plugin '{}'; skipping",
                    hook_name, manifest.name
                );
                continue;
            }

            let script_rel = Path::new(&entry.script);

            // Reject `..` escapes in the relative path before any join.
            {
                use std::path::Component;
                for comp in script_rel.components() {
                    if let Component::ParentDir = comp {
                        return Err(format!(
                            "hook script {:?} escapes the plugin directory",
                            entry.script
                        ));
                    }
                }
            }

            let script_abs = canon_dir.join(script_rel);

            // Canonicalize if possible to catch symlink escapes.
            let canon_script =
                std::fs::canonicalize(&script_abs).unwrap_or_else(|_| script_abs.clone());
            if !canon_script.starts_with(&canon_dir) {
                return Err(format!(
                    "hook script {:?} escapes the plugin directory",
                    entry.script
                ));
            }

            if !canon_script.exists() {
                return Err(format!("hook script {:?} does not exist", entry.script));
            }

            // Cross-platform executable check (Unix permission bit, or
            // extension/presence on Windows where there is no exec bit).
            let is_exe = is_executable(&canon_script);
            if !is_exe {
                return Err(format!(
                    "hook script {:?} is not executable (try chmod +x)",
                    entry.script
                ));
            }

            let timeout_ms = entry
                .timeout_ms
                .unwrap_or_else(|| default_hook_timeout(hook_name));

            hooks.insert(
                hook_name.clone(),
                HookConfig {
                    script: canon_script,
                    timeout_ms,
                    pass_args: entry.pass_args,
                },
            );
        }

        // --- plugin-declared tools (custom capabilities, no MCP needed) ---
        // Each tool's handler script gets the same path-confinement +
        // executable checks as a hook script, so a plugin can't reach outside
        // its directory or run a non-executable file. Reserved-name filtering
        // (collisions with built-in tools) is done by the caller at merge time,
        // not here — keep loading decoupled from the built-in tool set.
        let mut tools_vec: Vec<ToolConfig> = Vec::new();
        for t in &manifest.tools {
            if t.name.is_empty() {
                return Err("plugin declares a tool with an empty name".into());
            }
            let canon_script = validate_plugin_script(&canon_dir, &t.script)?;
            let timeout_ms = t.timeout_ms.unwrap_or(DEFAULT_POST_TIMEOUT_MS);
            let kind = match t.kind.as_deref().unwrap_or("destructive") {
                "readonly" => ToolKind::ReadOnly,
                "destructive" => ToolKind::Destructive,
                other => {
                    return Err(format!(
                        "tool '{}' has invalid kind '{}' (use 'readonly' or 'destructive')",
                        t.name, other
                    ))
                }
            };
            let parameters = if t.parameters.is_object() {
                t.parameters.clone()
            } else {
                json!({ "type": "object", "properties": {} })
            };
            tools_vec.push(ToolConfig {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters,
                script: canon_script,
                timeout_ms,
                kind,
                override_builtin: t.override_builtin,
                plugin_name: manifest.name.clone(),
            });
        }

        // --- plugin-declared slash commands ---
        let mut commands_vec: Vec<CommandConfig> = Vec::new();
        for c in &manifest.commands {
            let name = c.name.trim();
            if name.is_empty() {
                continue;
            }
            let name = name.strip_prefix('/').unwrap_or(name);
            if name.is_empty() {
                continue;
            }
            if RESERVED_COMMAND_NAMES.contains(&name) {
                return Err(format!(
                    "command '{name}' collides with a reserved builtin slash command"
                ));
            }
            // A command is either script-backed (`script`) or prompt-backed
            // (`prompt_file` -> agent turn). Exactly one must be set.
            let has_script = c
                .script
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some();
            let has_prompt = c
                .prompt_file
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some();
            let mode = match (has_script, has_prompt, c.mode.as_deref()) {
                (true, false, None | Some("script")) => CommandMode::Script,
                (false, true, None | Some("agent_turn")) => CommandMode::AgentTurn,
                (true, true, _) => {
                    return Err(format!(
                        "command '{name}' declares both `script` and `prompt_file`; use exactly one"
                    ));
                }
                (false, false, _) => {
                    return Err(format!(
                        "command '{name}' declares neither `script` nor `prompt_file`; set one"
                    ));
                }
                (true, false, Some(m)) => {
                    return Err(format!(
                        "command '{name}' has `script` but mode=\"{m}\" (expected \"script\")"
                    ));
                }
                (false, true, Some(m)) => {
                    return Err(format!(
                        "command '{name}' has `prompt_file` but mode=\"{m}\" (expected \"agent_turn\")"
                    ));
                }
            };
            let (script, prompt_file) = match mode {
                CommandMode::Script => {
                    let s = validate_plugin_script(&canon_dir, c.script.as_deref().unwrap())?;
                    (Some(s), None)
                }
                CommandMode::AgentTurn => {
                    let p = validate_prompt_file(&canon_dir, c.prompt_file.as_deref().unwrap())?;
                    (None, Some(p))
                }
            };
            commands_vec.push(CommandConfig {
                name: name.to_string(),
                description: c.description.clone(),
                mode,
                script,
                prompt_file,
                timeout_ms: c.timeout_ms.unwrap_or(DEFAULT_POST_TIMEOUT_MS),
                plugin_name: manifest.name.clone(),
            });
        }

        // --- plugin-declared OAuth provider (subscription auth, no recompile) ---
        let oauth_config = match manifest.oauth {
            Some(entry) => Some(load_oauth_entry(&canon_dir, entry)?),
            None => None,
        };

        // --- plugin-declared memory provider (replace markdown store) ---
        let memory_provider = match manifest.memory_provider {
            Some(entry) => {
                if entry.script.trim().is_empty() {
                    return Err("memory_provider.script is empty".into());
                }
                let script = validate_plugin_script(&canon_dir, &entry.script)?;
                Some(PluginMemoryProviderConfig {
                    plugin_name: manifest.name.clone(),
                    script,
                    timeout_ms: entry.timeout_ms.unwrap_or(DEFAULT_POST_TIMEOUT_MS),
                })
            }
            None => None,
        };

        Ok(Plugin {
            name: manifest.name,
            version: manifest.version,
            protocol_version: plugin_protocol_version,
            capabilities,
            description: manifest.description,
            enabled: true,
            source_path: canon_dir,
            hooks,
            tools: tools_vec,
            commands: commands_vec,
            disable_tools: manifest.disable_tools,
            system_prompt: manifest.system_prompt,
            oauth: oauth_config,
            memory_provider,
        })
    }

    /// Resolve the on-disk directory for a given install scope.
    pub fn install_dir_for(&self, scope: PluginInstallScope) -> Result<PathBuf, String> {
        match scope {
            PluginInstallScope::Global => self.user_plugins_dir.clone().ok_or_else(|| {
                "global plugin install unavailable (no ~/.catalyst-code/plugins)".into()
            }),
            // `plugins_dir` is absolute (resolved against workspace at construct).
            PluginInstallScope::Workspace => Ok(self.plugins_dir.clone()),
        }
    }

    /// Classify an installed plugin path as global vs workspace.
    pub fn scope_of_path(&self, path: &Path) -> PluginInstallScope {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let canon_ws =
            std::fs::canonicalize(&self.workspace).unwrap_or_else(|_| self.workspace.clone());
        if canon.starts_with(&canon_ws) {
            PluginInstallScope::Workspace
        } else {
            PluginInstallScope::Global
        }
    }

    fn record_user_installed(&self, path: &Path) -> Result<(), String> {
        let Some(store_path) = &self.trust_store_path else {
            return Ok(());
        };
        let _lock = crate::fsutil::FileLock::acquire(&store_path.with_extension("lock"))
            .map_err(|e| format!("failed to lock plugin trust store: {e}"))?;
        let mut store = crate::plugin_trust::PluginTrustStore::load(store_path);
        let mut installed = store.installed_plugins_for(&self.trust_key);
        installed.insert(canonical_plugin_key(path));
        store.set_installed_plugins(&self.trust_key, &installed);
        store.save(store_path)
    }

    /// True when repo-shipped workspace plugins are allowed to load. Explicitly
    /// user-installed workspace plugins are recorded in the user-owned store
    /// and do not require this blanket trust flag.
    pub fn trust_project(&self) -> bool {
        self.trust_project
    }

    /// Names of currently loaded workspace-scoped plugins. Global user-owned
    /// plugins are excluded.
    pub fn project_plugin_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .plugins
            .read()
            .unwrap()
            .values()
            .filter(|p| self.scope_of_path(&p.source_path) == PluginInstallScope::Workspace)
            .map(|p| p.name.clone())
            .collect();
        out.sort();
        out
    }

    /// Install from a local path or GitHub Release URL into the given scope.
    ///
    /// GitHub sources download the release source `.zip` (latest, or a pinned
    /// tag) so installs are versioned and can later be auto-updated by
    /// re-fetching a newer release. Local paths keep the existing copy-into-
    /// managed-dir behavior.
    ///
    /// An existing local directory with `plugin.json` always wins over
    /// `owner/repo` shorthand, so relative paths like `./plugins/foo` stay
    /// unambiguous.
    pub async fn install_source(
        &self,
        source: &str,
        scope: PluginInstallScope,
    ) -> Result<Plugin, String> {
        let source = source.trim();
        if source.is_empty() {
            return Err("plugin source is empty".into());
        }

        let local = Path::new(source);
        let local_resolved = if local.is_absolute() {
            local.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(local)
        };
        if local_resolved.is_dir() && local_resolved.join("plugin.json").is_file() {
            return self.install(&local_resolved, scope);
        }

        if let Some(gh) = parse_github_plugin_source(source) {
            let (plugin_dir, meta, tmp_root) = fetch_github_release_plugin(&gh).await?;
            let result = self.install(&plugin_dir, scope);
            // Always clean the temp extract tree (install copies into managed dir).
            let _ = std::fs::remove_dir_all(&tmp_root);
            let installed = result?;
            if let Err(e) = write_plugin_source_meta(&installed.source_path, &meta) {
                eprintln!(
                    "[plugins] warning: installed '{}' but could not write {}: {e}",
                    installed.name, PLUGIN_SOURCE_META_FILE
                );
            }
            return Ok(installed);
        }
        self.install(local, scope)
    }

    /// Install a plugin from `source_path` (a directory containing plugin.json).
    /// The plugin directory is copied into the scoped plugins directory and
    /// registered. Returns an error if a plugin with the same name already exists
    /// or if validation fails.
    pub fn install(&self, source_path: &Path, scope: PluginInstallScope) -> Result<Plugin, String> {
        let source = if source_path.is_absolute() {
            source_path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(source_path)
        };
        if !source.is_dir() {
            return Err(format!("{:?} is not a directory", source_path));
        }
        let manifest_path = source.join("plugin.json");
        if !manifest_path.exists() {
            return Err(format!("no plugin.json found in {:?}", source_path));
        }

        // Pre-validate the plugin from source before copying.
        let plugin = Self::load_plugin_from_dir(&source)?;

        // Check for name collision.
        {
            let plugins = self.plugins.read().unwrap();
            if plugins.contains_key(&plugin.name) {
                return Err(format!(
                    "plugin '{}' is already installed; remove it first or use a different name",
                    plugin.name
                ));
            }
        }

        let dest_root = self.install_dir_for(scope)?;
        let _ = std::fs::create_dir_all(&dest_root);
        Self::validate_plugin_name(&plugin.name)?;
        let dest_dir = dest_root.join(&plugin.name);
        if dest_dir.exists() {
            let _ = std::fs::remove_dir_all(&dest_dir);
        }

        copy_dir(&source, &dest_dir)?;

        // Workspace installs are recorded in the user-owned trust store; a
        // repo cannot self-authorize by committing a marker file next to its
        // manifest. Test-only managers retain their marker-based fixture
        // behavior because they intentionally have no persistent trust store.
        if scope == PluginInstallScope::Workspace
            || self.scope_of_path(&dest_dir) == PluginInstallScope::Workspace
        {
            if self.trust_store_path.is_some() {
                self.record_user_installed(&dest_dir)?;
            }
            #[cfg(test)]
            if self.trust_store_path.is_none() {
                write_user_installed_marker(&dest_dir);
            }
        }

        // Re-load from the copied location so paths point to the managed dir.
        let installed = Self::load_plugin_from_dir(&dest_dir)?;

        // Always register: global always loads; workspace user-installs carry
        // the marker so they load even without --trust-project-plugins.
        self.plugins
            .write()
            .unwrap()
            .insert(installed.name.clone(), installed.clone());

        Ok(installed)
    }

    /// Remove a plugin by name. Deletes the plugin directory from disk and
    /// unregisters it from the in-memory registry.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let mut plugins = self.plugins.write().unwrap();
        if let Some(plugin) = plugins.remove(name) {
            let _ = std::fs::remove_dir_all(&plugin.source_path);
            Ok(())
        } else {
            Err(format!("plugin '{}' not found", name))
        }
    }

    /// Enable a previously-disabled plugin by name.
    pub fn enable(&self, name: &str) -> Result<(), String> {
        let mut plugins = self.plugins.write().unwrap();
        if let Some(plugin) = plugins.get_mut(name) {
            plugin.enabled = true;
            Ok(())
        } else {
            Err(format!("plugin '{}' not found", name))
        }
    }

    /// Disable a plugin by name (keeps it on disk, stops invoking hooks).
    pub fn disable(&self, name: &str) -> Result<(), String> {
        let mut plugins = self.plugins.write().unwrap();
        if let Some(plugin) = plugins.get_mut(name) {
            plugin.enabled = false;
            Ok(())
        } else {
            Err(format!("plugin '{}' not found", name))
        }
    }

    /// Return a snapshot of all registered plugins (name → Plugin).
    pub fn list(&self) -> HashMap<String, Plugin> {
        self.plugins.read().unwrap().clone()
    }

    /// Enabled plugins sorted deterministically by name.
    fn enabled_plugins_sorted(&self) -> Vec<Plugin> {
        let mut out: Vec<Plugin> = self
            .plugins
            .read()
            .unwrap()
            .values()
            .filter(|p| p.enabled)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Get all enabled hook configs for a given hook point name.
    /// Returns a vec of (plugin_name, HookConfig) pairs so the caller can
    /// iterate and merge results.
    ///
    /// Plugins are returned in deterministic alphabetical order by plugin name
    /// so multi-plugin composition is predictable and testable.
    pub fn get_hook_configs(&self, hook_name: &str) -> Vec<(String, HookConfig)> {
        let mut out: Vec<(String, HookConfig)> = self
            .plugins
            .read()
            .unwrap()
            .values()
            .filter(|p| p.enabled)
            .filter_map(|p| p.hooks.get(hook_name).map(|c| (p.name.clone(), c.clone())))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Cheap existence check (no config clone): does any enabled plugin register
    /// this hook point? Used to decide whether to clone tool args before the
    /// pre-hook phase without paying for a full `get_hook_configs`.
    pub fn has_hook(&self, hook_name: &str) -> bool {
        self.plugins
            .read()
            .unwrap()
            .values()
            .filter(|p| p.enabled)
            .any(|p| p.hooks.contains_key(hook_name))
    }

    /// Look up a single plugin by name.
    pub fn get_plugin(&self, name: &str) -> Option<Plugin> {
        self.plugins.read().unwrap().get(name).cloned()
    }

    /// OpenAI function-calling tool definitions for every tool declared by
    /// ENABLED plugins. Built-in tools are NOT included here; the caller merges
    /// them and filters name collisions (a plugin tool may never shadow a
    /// built-in). Empty when no plugin declares tools.
    pub fn tool_definitions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for p in self.enabled_plugins_sorted() {
            for t in &p.tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                }));
            }
        }
        out
    }

    /// Look up a plugin-declared tool's config by tool name (enabled plugins
    /// only). Returns None for built-in tools.
    pub fn tool_config(&self, name: &str) -> Option<ToolConfig> {
        self.enabled_plugins_sorted()
            .into_iter()
            .find_map(|p| p.tools.iter().find(|t| t.name == name).cloned())
    }

    /// Slash-command definitions for every command declared by ENABLED plugins.
    /// Each entry is `{name, description, plugin}`.
    pub fn command_definitions(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for p in self.enabled_plugins_sorted() {
            for c in &p.commands {
                out.push(json!({
                    "name": c.name,
                    "description": c.description,
                    "plugin": p.name,
                    "mode": c.mode.as_str(),
                }));
            }
        }
        out
    }

    /// Look up a plugin-declared slash command by name (enabled plugins only).
    /// Accepts names with or without a leading `/`.
    pub fn command_config(&self, name: &str) -> Option<CommandConfig> {
        let name = name.strip_prefix('/').unwrap_or(name);
        self.enabled_plugins_sorted()
            .into_iter()
            .find_map(|p| p.commands.iter().find(|c| c.name == name).cloned())
    }

    /// Re-scan plugin directories, preserving per-plugin `enabled` flags for
    /// plugins that are still present. Prunes OAuth token cache entries for
    /// providers that disappeared. Returns a JSON summary:
    /// `{loaded, skipped, errors?}`.
    pub fn reload(&self) -> Value {
        let enabled_map: HashMap<String, bool> = self
            .plugins
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.enabled))
            .collect();

        let errors = self.scan_and_load();

        // Re-apply previously-disabled flags for plugins that still exist.
        {
            let mut plugins = self.plugins.write().unwrap();
            for (name, enabled) in &enabled_map {
                if !*enabled {
                    if let Some(p) = plugins.get_mut(name) {
                        p.enabled = false;
                    }
                }
            }
        }

        // Prune token_cache for OAuth providers that are no longer loaded.
        {
            let active: std::collections::HashSet<String> = self
                .plugins
                .read()
                .unwrap()
                .values()
                .filter_map(|p| p.oauth.as_ref().map(|o| o.provider_id.clone()))
                .collect();
            let mut cache = self.token_cache.lock().unwrap();
            cache.retain(|k, _| active.contains(k));
        }

        let loaded = self.plugins.read().unwrap().len();
        let skipped = self
            .skipped_project
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<String>>();
        let mut out = json!({
            "loaded": loaded,
            "skipped": skipped,
        });
        if !errors.is_empty() {
            out.as_object_mut()
                .unwrap()
                .insert("errors".into(), json!(errors));
        }
        out
    }

    /// Approval classification for a plugin-declared tool, if it exists.
    pub fn tool_kind(&self, name: &str) -> Option<ToolKind> {
        self.tool_config(name).map(|t| t.kind)
    }

    /// Union of tool names every enabled plugin asks to disable (the
    /// `disable_tools` manifest field). Applied as a FINAL filter on the
    /// model's tool list, so a disabled name is gone whether it's a built-in
    /// or an override — `disable_tools` is the strongest "remove a feature"
    /// lever and always wins over `override`.
    pub fn disabled_tools(&self) -> std::collections::HashSet<String> {
        self.enabled_plugins_sorted()
            .into_iter()
            .flat_map(|p| p.disable_tools.into_iter())
            .collect()
    }

    /// Built-in tool names for which an enabled plugin declares an
    /// `override: true` tool — the plugin's handler replaces the built-in's
    /// implementation. A plugin tool named like a built-in WITHOUT
    /// `override: true` does NOT appear here (it stays a no-op collision,
    /// built-in wins — unchanged behavior).
    pub fn overridden_tool_names(&self) -> std::collections::HashSet<String> {
        self.enabled_plugins_sorted()
            .into_iter()
            .flat_map(|p| p.tools.into_iter())
            .filter(|t| t.override_builtin && crate::tools::is_builtin(&t.name))
            .map(|t| t.name)
            .collect()
    }

    /// Concatenated `system_prompt` text from every enabled plugin that
    /// declares one, each framed with its plugin name + version. Empty (so the
    /// system prompt + its prefix cache are untouched) when no plugin declares
    /// any. Lets a plugin inject domain rules / persona / context into the
    /// system prompt — the same surface a core edit of SYSTEM_PROMPT_BASE
    /// touches.
    ///
    /// Order is deterministic by plugin name so multi-plugin system prompt
    /// composition is stable.
    pub fn system_prompt_injection(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for p in self.enabled_plugins_sorted() {
            let s = p.system_prompt.trim();
            if !s.is_empty() {
                parts.push(format!("# Plugin: {} (v{})\n{}", p.name, p.version, s));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\n\n## Plugin-injected context\n\n{}", parts.join("\n\n"))
        }
    }

    /// The active memory provider, if any enabled plugin declares one.
    /// First enabled plugin (sorted by name) that declares `memory_provider`
    /// wins — only one provider should be active; later ones are ignored.
    pub fn memory_provider(&self) -> Option<PluginMemoryProviderConfig> {
        self.enabled_plugins_sorted()
            .into_iter()
            .find_map(|p| p.memory_provider)
    }

    /// True when an enabled plugin replaces the built-in markdown memory store.
    pub fn has_memory_provider(&self) -> bool {
        self.memory_provider().is_some()
    }

    // ---- OAuth provider declarations (subscription auth, no recompile) ----

    /// All OAuth providers declared by enabled plugins (one per plugin that
    /// declares an `oauth` block). Used to populate the `/login` picker and to
    /// dispatch `/login` / optional `/oauth-code` / `/logout`.
    pub fn oauth_configs(&self) -> Vec<PluginOauthConfig> {
        self.enabled_plugins_sorted()
            .into_iter()
            .filter_map(|p| p.oauth)
            .collect()
    }

    /// Look up a plugin-declared OAuth provider by its provider_id.
    pub fn oauth_config(&self, provider_id: &str) -> Option<PluginOauthConfig> {
        self.enabled_plugins_sorted()
            .into_iter()
            .find_map(|p| p.oauth.filter(|o| o.provider_id == provider_id))
    }

    /// Find the plugin OAuth provider that should authenticate a resolved
    /// provider at turn time. Matches by provider-config name == provider_id
    /// first (the `/login` flow creates the config with that name), then by
    /// base_url host (a manually-configured provider pointing at the plugin's
    /// declared endpoint).
    pub fn oauth_config_for_provider(&self, rp: &ResolvedProvider) -> Option<PluginOauthConfig> {
        let configs = self.oauth_configs();
        if let Some(c) = configs.iter().find(|c| c.provider_id == rp.name) {
            return Some(c.clone());
        }
        if let (Some(host),) = (url_host(&rp.base_url),) {
            if let Some(c) = configs
                .iter()
                .find(|c| url_host(&c.base_url).as_deref() == Some(host.as_str()))
            {
                return Some(c.clone());
            }
        }
        None
    }

    /// Build the `ProviderConfig` to create on a successful `/login` for a
    /// plugin OAuth provider (no api_key — the token is resolved + refreshed at
    /// turn time). `finalize_oauth` uses this in place of a built-in preset.
    pub fn oauth_provider_config(&self, provider_id: &str) -> Option<ProviderConfig> {
        let cfg = self.oauth_config(provider_id)?;
        Some(ProviderConfig {
            name: cfg.provider_id.clone(),
            kind: cfg.kind,
            base_url: cfg.base_url.clone(),
            api_key: None,
            api_key_env: None,
            headers: cfg.headers.clone(),
            context_window: None,
            models_override: Vec::new(),
            models_endpoint: None,
        })
    }

    /// True when a plugin declares an OAuth login flow for `provider_id` (the
    /// login action has a resolvable script).
    pub fn supports_oauth_login(&self, provider_id: &str) -> bool {
        let Some(cfg) = self.oauth_config(provider_id) else {
            return false;
        };
        cfg.script_for("login").is_some()
    }

    /// Cheap sync check (no subprocess): does the plugin's token file or its
    /// optional external credential file exist?
    /// Used to gate an OAuth-only provider into model aggregation so `/models`
    /// shows it without an API key.
    pub fn has_oauth_creds(&self, provider_id: &str) -> bool {
        self.oauth_config(provider_id)
            .map(|c| {
                c.token_path.is_file()
                    || c.detect_path.as_ref().map(|p| p.is_file()).unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// Delete the plugin's stored token + invalidate the cache. Called by
    /// `/logout` so the provider is fully logged out (not just its config).
    /// Best-effort: also invokes the plugin's `clear` action so the plugin can
    /// tear down any extra state it manages.
    pub async fn clear_oauth(&self, provider_id: &str) {
        if let Some(cfg) = self.oauth_config(provider_id) {
            let _ = std::fs::remove_file(&cfg.token_path);
            if let Some(script) = cfg.script_for("clear") {
                let ctx =
                    self.oauth_action_ctx("clear", provider_id, &cfg.token_path.to_string_lossy());
                let _ = self
                    .execute_oauth_script(
                        script,
                        ctx,
                        cfg.token_timeout_ms,
                        &oauth_script_env(&cfg),
                    )
                    .await;
            }
        }
        if let Ok(mut cache) = self.token_cache.lock() {
            cache.remove(provider_id);
        }
    }

    /// Resolve fresh (cached) OAuth credentials for `provider_id` at turn /
    /// discovery time. Spawns the plugin's `token` action only when the cached
    /// token is missing or near expiry. Returns None when no creds exist or the
    /// script fails — callers fall back to the API-key path (no regression).
    ///
    /// The script may also return `"headers": [["Name","value"], …]` which are
    /// cached with the token and merged onto the resolved provider.
    pub async fn resolve_oauth_creds(&self, provider_id: &str) -> Option<ResolvedOauthCreds> {
        let cfg = self.oauth_config(provider_id)?;
        // Cache hit?
        {
            let cache = self.token_cache.lock().ok()?;
            if let Some(c) = cache.get(provider_id) {
                let now = now_secs();
                if c.expires_at == 0 || c.expires_at > now + 60 {
                    return Some(ResolvedOauthCreds {
                        access_token: c.token.clone(),
                        headers: c.headers.clone(),
                    });
                }
            }
        }
        let script = cfg.script_for("token")?;
        let ctx = self.oauth_action_ctx("token", provider_id, &cfg.token_path.to_string_lossy());
        // Log failures instead of swallowing them: a silently-failing token
        // script surfaces to the user as an unexplained "run /login" prompt.
        let resp = match self
            .execute_oauth_script(script, ctx, cfg.token_timeout_ms, &oauth_script_env(&cfg))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[plugins] oauth token script for '{provider_id}' failed: {e}");
                return None;
            }
        };
        let token = match resp
            .get("access_token")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
        {
            Some(t) => t,
            None => {
                eprintln!(
                    "[plugins] oauth token script for '{provider_id}' returned no access_token (re-login may be required)"
                );
                return None;
            }
        };
        let headers = parse_oauth_token_headers(resp.get("headers"));
        let now = now_secs();
        let exp = resp
            .get("expires_at")
            .and_then(|e| e.as_u64())
            .filter(|e| *e > 0)
            .unwrap_or(now + 300);
        if let Ok(mut cache) = self.token_cache.lock() {
            cache.insert(
                provider_id.to_string(),
                CachedToken {
                    token: token.clone(),
                    expires_at: exp,
                    headers: headers.clone(),
                },
            );
        }
        Some(ResolvedOauthCreds {
            access_token: token,
            headers,
        })
    }

    /// Convenience wrapper: access token only (no headers).
    pub async fn resolve_oauth_token(&self, provider_id: &str) -> Option<String> {
        self.resolve_oauth_creds(provider_id)
            .await
            .map(|c| c.access_token)
    }

    /// Drive the interactive OAuth login for a plugin provider. Picks the flow
    /// from the script's returned `flow` field:
    ///  - `web` (default for a local machine): the harness binds a loopback
    ///    redirect, the script builds the authorize URL with that redirect_uri,
    ///    the harness waits for the browser redirect, then calls `complete`.
    ///  - `manual` (default for SSH/headless, or when the script chooses it):
    ///    the script returns a URL + an opaque `pending` blob; the user pastes
    ///    the code back via `/oauth-code`, which calls `complete`.
    ///  - `poll` / `auto`: the script returns a device-code URL + pending blob,
    ///    and the harness immediately invokes `complete`; the script owns the
    ///    polling loop, so no `/oauth-code` command is needed.
    ///  - `already_authenticated`: the script imported an existing credential
    ///    store and no browser flow is needed.
    pub async fn oauth_login(
        &self,
        provider_id: &str,
        emit: &(dyn Fn(OAuthPrompt) + Send + Sync),
    ) -> Result<LoginOutcome, String> {
        let cfg = self
            .oauth_config(provider_id)
            .ok_or_else(|| format!("'{provider_id}' has no plugin OAuth login flow"))?;
        let token_path = cfg.token_path.to_string_lossy().to_string();
        let headless = crate::oauth::likely_headless();

        if !headless {
            // Web flow: bind a loopback redirect the script embeds in its URL.
            // Plugins override ``redirect_path`` when their OAuth provider
            // requires a specific registered path (e.g. Google's
            // installed-app OAuth clients require ``/oauth2callback``).
            let (listener, listener_v6, port) = crate::oauth::bind_loopback(0).await?;
            let redirect_uri = format!(
                "http://localhost:{port}{}",
                if cfg.redirect_path.starts_with('/') {
                    cfg.redirect_path.clone()
                } else {
                    format!("/{}", cfg.redirect_path)
                }
            );
            let mut ctx = self.oauth_action_ctx("login", provider_id, &token_path);
            ctx["headless"] = json!(false);
            ctx["redirect_uri"] = json!(redirect_uri);
            let resp = self
                .execute_oauth_script(
                    cfg.script_for("login").ok_or("no login script")?,
                    ctx,
                    cfg.login_timeout_ms,
                    &oauth_script_env(&cfg),
                )
                .await?;
            if let Some(err) = oauth_login_error(&resp) {
                return Err(format!("OAuth login failed: {err}"));
            }
            let flow = oauth_flow(&resp, "web");
            let state = resp
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let pending = resp.get("pending").cloned();
            if oauth_flow_is_existing(&resp) {
                self.invalidate_oauth_cache(provider_id);
                return Ok(LoginOutcome::Done);
            }
            let url = resp
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("login script did not return a url")?
                .to_string();
            let code = resp.get("code").and_then(|v| v.as_str()).map(String::from);
            let message = resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Open the URL to log in.")
                .to_string();
            emit(OAuthPrompt {
                url: url.clone(),
                code,
                message,
            });
            let _ = crate::oauth::open_browser(&url);

            if oauth_flow_is_auto(&resp) {
                self.run_oauth_complete(
                    provider_id,
                    &token_path,
                    "",
                    pending.as_ref(),
                    Some(&redirect_uri),
                )
                .await?;
                return Ok(LoginOutcome::Done);
            }
            if flow == "manual" {
                // The script insisted on the manual flow even locally.
                return Ok(LoginOutcome::AwaitingCode {
                    pending: PendingOauth::plugin(provider_id, state, pending),
                });
            }
            // Wait for the browser redirect, then complete the exchange.
            let code =
                crate::oauth::await_redirect_dual(listener, listener_v6, &state, None).await?;
            self.run_oauth_complete(
                provider_id,
                &token_path,
                &code,
                pending.as_ref(),
                Some(&redirect_uri),
            )
            .await?;
            Ok(LoginOutcome::Done)
        } else {
            // Headless flow: providers may still choose manual paste, but a
            // device-code provider can return `flow: "poll"` and complete
            // itself while the harness waits for authorization.
            let mut ctx = self.oauth_action_ctx("login", provider_id, &token_path);
            ctx["headless"] = json!(true);
            let resp = self
                .execute_oauth_script(
                    cfg.script_for("login").ok_or("no login script")?,
                    ctx,
                    cfg.login_timeout_ms,
                    &oauth_script_env(&cfg),
                )
                .await?;
            if let Some(err) = oauth_login_error(&resp) {
                return Err(format!("OAuth login failed: {err}"));
            }
            if oauth_flow_is_existing(&resp) {
                self.invalidate_oauth_cache(provider_id);
                return Ok(LoginOutcome::Done);
            }
            let url = resp
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("login script did not return a url")?
                .to_string();
            let code = resp.get("code").and_then(|v| v.as_str()).map(String::from);
            let message = resp
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Open the URL, approve, then paste the code via /oauth-code.")
                .to_string();
            let pending = resp.get("pending").cloned();
            emit(OAuthPrompt { url, code, message });
            if oauth_flow_is_auto(&resp) {
                self.run_oauth_complete(provider_id, &token_path, "", pending.as_ref(), None)
                    .await?;
                return Ok(LoginOutcome::Done);
            }
            Ok(LoginOutcome::AwaitingCode {
                pending: PendingOauth::plugin(provider_id, String::new(), pending),
            })
        }
    }

    /// Run a plugin's `complete` action for either a pasted code, a loopback
    /// redirect, or an automatic device-code poll.
    async fn run_oauth_complete(
        &self,
        provider_id: &str,
        token_path: &str,
        code: &str,
        pending: Option<&Value>,
        redirect_uri: Option<&str>,
    ) -> Result<(), String> {
        let cfg = self
            .oauth_config(provider_id)
            .ok_or_else(|| format!("'{provider_id}' has no plugin OAuth flow"))?;
        let script = cfg
            .script_for("complete")
            .ok_or_else(|| format!("'{provider_id}' has no complete script"))?;
        let mut ctx = self.oauth_action_ctx("complete", provider_id, token_path);
        ctx["code"] = json!(code);
        if let Some(uri) = redirect_uri {
            ctx["redirect_uri"] = json!(uri);
        }
        if let Some(p) = pending {
            ctx["pending"] = p.clone();
        }
        let resp = self
            .execute_oauth_script(script, ctx, cfg.login_timeout_ms, &oauth_script_env(&cfg))
            .await?;
        if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let err = resp
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("OAuth complete failed: {err}"));
        }
        self.invalidate_oauth_cache(provider_id);
        Ok(())
    }

    /// Complete a pending manual (paste-code) plugin OAuth login: exchange the
    /// code for a token via the plugin's `complete` action. The plugin writes
    /// the token to its token_path; the harness never parses the token format.
    pub async fn oauth_complete(
        &self,
        provider_id: &str,
        pending: &PendingOauth,
        code: &str,
    ) -> Result<(), String> {
        let cfg = self
            .oauth_config(provider_id)
            .ok_or_else(|| format!("'{provider_id}' has no plugin OAuth flow"))?;
        let token_path = cfg.token_path.to_string_lossy().to_string();
        self.run_oauth_complete(
            provider_id,
            &token_path,
            code,
            pending.plugin_pending.as_ref(),
            None,
        )
        .await
    }

    fn invalidate_oauth_cache(&self, provider_id: &str) {
        if let Ok(mut cache) = self.token_cache.lock() {
            cache.remove(provider_id);
        }
    }

    /// Build the base context JSON passed to every OAuth script invocation
    /// (action-specific fields are added by the caller via `ctx["..."] = ...`).
    fn oauth_action_ctx(&self, action: &str, provider_id: &str, token_path: &str) -> Value {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        json!({
            "action": action,
            "provider_id": provider_id,
            "token_path": token_path,
            "workspace": self.workspace.to_string_lossy(),
            "timestamp": timestamp,
        })
    }

    /// Spawn an OAuth script, write the context JSON to its stdin, read one
    /// JSON object from stdout. Bounded by `timeout_ms` (stdin-write + wait).
    /// Mirrors `execute_hook` / `execute_plugin_tool` (kill_on_drop, bounded
    /// stdin write, timeout). Non-zero exit / timeout / parse failure → Err.
    async fn execute_oauth_script(
        &self,
        script: &Path,
        context: Value,
        timeout_ms: u64,
        extra_env: &[(String, String)],
    ) -> Result<Value, String> {
        let ctx_bytes = serde_json::to_vec(&context).unwrap_or_default();
        validate_plugin_io("oauth script", ctx_bytes.len())?;
        let mut child = match hook_command_with_env(script, extra_env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("failed to spawn oauth script {script:?}: {e}")),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let stdin_timeout = Duration::from_millis(timeout_ms.max(1000));
            let write_fut = async {
                let _ = stdin.write_all(&ctx_bytes).await;
                let _ = stdin.shutdown().await;
            };
            if tokio::time::timeout(stdin_timeout, write_fut)
                .await
                .is_err()
            {
                let _ = child.start_kill();
                return Err(format!(
                    "oauth script did not consume stdin within {}ms",
                    stdin_timeout.as_millis()
                ));
            }
        }
        let timeout_dur = Duration::from_millis(timeout_ms);
        match bounded_plugin_output(child, timeout_dur).await {
            Ok(Ok(output)) => {
                validate_plugin_output("oauth script", &output.stdout, &output.stderr)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!(
                        "oauth script exited with {}: {}",
                        output.status,
                        stderr.trim()
                    ));
                }
                let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if stdout.is_empty() {
                    return Err("oauth script returned empty stdout".into());
                }
                serde_json::from_str::<Value>(&stdout)
                    .map_err(|e| format!("oauth script returned invalid JSON: {e}"))
            }
            Ok(Err(e)) => Err(format!("oauth script wait error: {e}")),
            Err(_) => Err(format!("oauth script timed out after {}ms", timeout_ms)),
        }
    }
}

// ---- hook execution ----

/// Execute a single hook script and return its result.
///
/// The hook receives `context` JSON on stdin. It must write a JSON response
/// (see the `plugin-authoring` skill for schema) to stdout. The function handles timeouts,
/// non-zero exits, and parse failures according to the safety rules:
///
/// - **pre_* hooks**: non-zero exit, timeout, or parse failure → deny
/// - **post_* / lifecycle hooks**: non-zero exit, timeout, or parse failure → skip
///
/// The `hook_name` prefix ("pre_" vs "post_" etc.) determines the safety rule.
/// Disabled plugin checks are handled before calling this function.
pub async fn execute_hook(
    hook_name: &str,
    plugin_name: &str,
    config: &HookConfig,
    context: &Value,
) -> HookResult {
    let policy = hook_policy(hook_name);
    let is_deny = policy.fail_mode == HookFailMode::Deny;

    let deny = |reason: String| HookResult {
        allow: false,
        reason,
        modify: None,
        notify: None,
        status: None,
    };

    let skip = |reason: String| HookResult {
        allow: true,
        reason: format!("[{plugin_name}] {reason}"),
        modify: None,
        notify: None,
        status: None,
    };

    // Spawn the hook script.
    let script_path = &config.script;
    let context_bytes = serde_json::to_vec(context).unwrap_or_default();
    if let Err(message) = validate_plugin_io("plugin hook", context_bytes.len()) {
        return if is_deny {
            deny(message)
        } else {
            skip(message)
        };
    }
    let timeout_dur = Duration::from_millis(config.timeout_ms);
    // Sandboxed (microVM) or host: plugin_run routes through the shared
    // execution backend when sandboxing is enabled, translating the script
    // path to its guest mount and denying host secrets. Same return shape as
    // bounded_plugin_output, so the deny/skip/error match arms are unchanged.
    let output_result = plugin_run(script_path, context_bytes, timeout_dur, &[]).await;

    match output_result {
        Ok(Ok(output)) => {
            if let Err(message) =
                validate_plugin_output("plugin hook", &output.stdout, &output.stderr)
            {
                return if is_deny {
                    deny(message)
                } else {
                    skip(message)
                };
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let msg = format!(
                    "hook '{}' exited with {}: {}",
                    hook_name,
                    output.status,
                    stderr.trim()
                );
                return if is_deny { deny(msg) } else { skip(msg) };
            }

            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                let msg = format!("hook '{}' returned empty stdout", hook_name);
                return if is_deny { deny(msg) } else { skip(msg) };
            }

            let response: Value = match serde_json::from_str(&stdout) {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("hook '{}' returned invalid JSON: {e}", hook_name);
                    return if is_deny { deny(msg) } else { skip(msg) };
                }
            };

            let allow = response
                .get("allow")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let reason = response
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let modify = response.get("modify").cloned();
            let notify = response
                .get("notify")
                .and_then(|v| v.as_str())
                .map(String::from);
            // Present key (including empty string = clear) → Some; absent → None.
            let status = if response.get("status").is_some() {
                Some(
                    response
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            } else {
                None
            };

            // Per-hook policy decides whether `allow`/`modify` are honored. For
            // blocking hooks (pre_*) a deny cancels the operation; for advisory
            // hooks (post_*, lifecycle) `allow:false` is logged as an observation
            // but never rolls back the operation.
            let effective_allow = if policy.honor_allow { allow } else { true };
            let effective_reason = if !policy.honor_allow && !reason.is_empty() {
                format!("[{plugin_name}] {reason}")
            } else {
                reason
            };
            let effective_modify = if policy.honor_modify { modify } else { None };
            let result = HookResult {
                allow: effective_allow,
                reason: effective_reason,
                modify: effective_modify,
                notify,
                status,
            };
            emit_plugin_ui_side_effects(
                plugin_name,
                result.notify.as_deref(),
                result.status.as_deref(),
            );
            result
        }
        Ok(Err(e)) => {
            let msg = format!("hook '{}' wait error: {e}", hook_name);
            if is_deny {
                deny(msg)
            } else {
                skip(msg)
            }
        }
        Err(_elapsed) => {
            let msg = format!(
                "hook '{}' timed out after {}ms",
                hook_name, config.timeout_ms
            );
            if is_deny {
                deny(msg)
            } else {
                skip(msg)
            }
        }
    }
}

/// Emit optional UI side effects (`notify` → info event, `status` → plugin_status).
/// `status: Some("")` clears the status text; absent status is a no-op.
pub fn emit_plugin_ui_side_effects(plugin_name: &str, notify: Option<&str>, status: Option<&str>) {
    use crate::protocol::{emit, Event};
    if let Some(msg) = notify.filter(|s| !s.is_empty()) {
        emit(&Event::new("info").with("message", json!(format!("[{plugin_name}] {msg}"))));
    }
    if let Some(text) = status {
        emit(
            &Event::new("plugin_status")
                .with("plugin", json!(plugin_name))
                .with("text", json!(text)),
        );
    }
}

/// Build the standard hook context JSON object.
///
/// The caller supplies the hook point name, tool name (empty string for
/// lifecycle hooks), workspace path, optional tool args, and session id.
/// If `pass_args` is false, the `args` field is omitted from the context.
pub fn build_context(
    hook_name: &str,
    tool_name: &str,
    workspace: &str,
    args: Option<&Value>,
    session_id: &str,
    pass_args: bool,
) -> Value {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut ctx = serde_json::json!({
        "hook": hook_name,
        "tool": tool_name,
        "workspace": workspace,
        "session_id": session_id,
        "timestamp": timestamp,
    });

    if pass_args {
        if let Some(a) = args {
            if let Some(obj) = ctx.as_object_mut() {
                obj.insert("args".to_string(), a.clone());
            }
        }
    }

    ctx
}

/// Merge a hook's `modify` object over the running tool args, in place.
///
/// `modify` is a JSON object whose keys override the corresponding keys in
/// `args` (shallow, per-key). Keys absent from `modify` are left untouched, so
/// a pre_write hook can return `{"content": "..."}` to reformat content while
/// preserving `path`/`edits`, a pre_bash hook can return `{"command": "..."}`
/// to fix a command, and a pre_read hook can return `{"path": "..."}` to
/// redirect a read. Non-object `modify` (or non-object `args`) is a no-op so a
/// malformed hook never corrupts the tool call.
pub fn apply_modify(args: &mut Value, modify: &Value) {
    if let (Some(base), Some(over)) = (args.as_object_mut(), modify.as_object()) {
        for (k, v) in over {
            base.insert(k.clone(), v.clone());
        }
    }
}

/// Execute a plugin-declared tool by spawning its handler script.
///
/// The handler receives one JSON object on stdin:
/// ```json
/// { "args": {…}, "workspace": "/abs/path", "session_id": "x.jsonl", "timestamp": 1719000000 }
/// ```
/// It must write one JSON object to stdout:
/// ```json
/// { "ok": true,  "output": "result text shown to the model" }
/// { "ok": false, "output": "error message" }      // `ok` omitted defaults to true
/// ```
/// A bare non-JSON stdout is accepted as the output text with `ok=true` (so a
/// trivial `echo` handler works). Non-zero exit, timeout, or spawn failure
/// produce an error `Outcome` — the conversation continues; the tool call
/// failed from the model's point of view. Safety mirrors `execute_hook`:
/// stdin-write is bounded by the tool's timeout, and the child is
/// `kill_on_drop` so a dropped future frees it.
pub async fn execute_plugin_tool(
    tool_name: &str,
    config: &ToolConfig,
    args: &Value,
    workspace: &str,
    session_id: &str,
) -> Outcome {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ctx = json!({
        "args": args,
        "workspace": workspace,
        "session_id": session_id,
        "timestamp": timestamp,
    });
    let ctx_bytes = serde_json::to_vec(&ctx).unwrap_or_default();
    if let Err(message) = validate_plugin_io("plugin tool", ctx_bytes.len()) {
        return Outcome::err(message);
    }

    let timeout_dur = Duration::from_millis(config.timeout_ms);
    match plugin_run(&config.script, ctx_bytes, timeout_dur, &[]).await {
        Ok(Ok(output)) => {
            if let Err(message) =
                validate_plugin_output("plugin tool", &output.stdout, &output.stderr)
            {
                return Outcome::err(message);
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Outcome::err(format!(
                    "plugin tool '{}' handler exited with {}: {}",
                    tool_name,
                    output.status,
                    stderr.trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                return Outcome::err(format!(
                    "plugin tool '{}' handler returned empty stdout",
                    tool_name
                ));
            }
            // Structured {ok, output} | {error} | {result}; else accept raw text.
            match serde_json::from_str::<Value>(&stdout) {
                Ok(v) if v.is_object() => {
                    let notify = v.get("notify").and_then(|n| n.as_str());
                    let status = if v.get("status").is_some() {
                        Some(v.get("status").and_then(|s| s.as_str()).unwrap_or(""))
                    } else {
                        None
                    };
                    emit_plugin_ui_side_effects(&config.plugin_name, notify, status);

                    if let Some(err) = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        Outcome::err(format!("plugin tool '{}': {}", tool_name, err))
                    } else {
                        let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(true);
                        let output_text = v
                            .get("output")
                            .or_else(|| v.get("result"))
                            .and_then(|o| o.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| stdout.clone());
                        if ok {
                            Outcome::ok(output_text)
                        } else {
                            Outcome::err(output_text)
                        }
                    }
                }
                Ok(_) => Outcome::ok(stdout),
                Err(_) => Outcome::ok(stdout),
            }
        }
        Ok(Err(e)) => Outcome::err(format!(
            "plugin tool '{}' handler wait error: {e}",
            tool_name
        )),
        Err(_) => Outcome::err(format!(
            "plugin tool '{}' handler timed out after {}ms",
            tool_name, config.timeout_ms
        )),
    }
}

/// Execute a plugin-declared slash command by spawning its handler script.
///
/// Stdin JSON:
/// `{ "command", "args", "workspace", "session_id", "timestamp", "plugin" }`
///
/// Stdout JSON:
/// `{ "ok": true|false, "output": "...", "notify"?: "...", "status"?: "..." }`
/// Non-JSON stdout is accepted as output text with `ok=true`.
/// Substitute the supported `{{token}}` placeholders in a command template.
///
/// Tokens are matched as `{{key}}` or `{{ key }}` (whitespace-tolerant),
/// case-sensitively. Supported keys: `args`, `workspace`, `session_id`,
/// `timestamp`, `plugin`, `command`. Unknown `{{...}}` sequences are left
/// verbatim so template authors can use literal double-braces freely.
pub fn render_command_template(
    template: &str,
    args: &str,
    workspace: &str,
    session_id: &str,
    timestamp: u64,
    plugin: &str,
    command: &str,
) -> String {
    let ts = timestamp.to_string();
    template
        .replace("{{ args }}", args)
        .replace("{{args}}", args)
        .replace("{{ workspace }}", workspace)
        .replace("{{workspace}}", workspace)
        .replace("{{ session_id }}", session_id)
        .replace("{{session_id}}", session_id)
        .replace("{{ timestamp }}", &ts)
        .replace("{{timestamp}}", &ts)
        .replace("{{ plugin }}", plugin)
        .replace("{{plugin}}", plugin)
        .replace("{{ command }}", command)
        .replace("{{command}}", command)
}

/// Render a prompt-backed command's template file with the invocation context
/// and return the prompt text to submit as an agent turn. Reads the
/// `prompt_file` resolved at load time (path-confined to the plugin dir) and
/// substitutes `{{args}}`, `{{workspace}}`, `{{session_id}}`, `{{timestamp}}`,
/// `{{plugin}}`, `{{command}}`. The rendered output is capped at
/// `MAX_PROMPT_COMMAND_BYTES`.
pub fn render_prompt_command(
    config: &CommandConfig,
    args: &str,
    workspace: &str,
    session_id: &str,
) -> Result<String, String> {
    let prompt_file = config
        .prompt_file
        .as_ref()
        .ok_or_else(|| format!("command '{}' is not prompt-backed", config.name))?;
    let template = std::fs::read_to_string(prompt_file).map_err(|e| {
        format!(
            "prompt_file {:?} for command '{}' could not be read: {e}",
            prompt_file, config.name
        )
    })?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rendered = render_command_template(
        &template,
        args,
        workspace,
        session_id,
        timestamp,
        &config.plugin_name,
        &config.name,
    );
    if rendered.len() > MAX_PROMPT_COMMAND_BYTES {
        return Err(format!(
            "rendered prompt for command '{}' exceeds the {} byte limit",
            config.name, MAX_PROMPT_COMMAND_BYTES
        ));
    }
    Ok(rendered)
}

pub async fn execute_plugin_command(
    config: &CommandConfig,
    args: &str,
    workspace: &str,
    session_id: &str,
) -> Outcome {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ctx = json!({
        "command": config.name,
        "args": args,
        "workspace": workspace,
        "session_id": session_id,
        "timestamp": timestamp,
        "plugin": config.plugin_name,
    });
    let ctx_bytes = serde_json::to_vec(&ctx).unwrap_or_default();
    if let Err(message) = validate_plugin_io("plugin command", ctx_bytes.len()) {
        return Outcome::err(message);
    }

    let Some(script) = config.script.as_ref() else {
        return Outcome::err(format!(
            "plugin command '{}' is not script-backed",
            config.name
        ));
    };
    let timeout_dur = Duration::from_millis(config.timeout_ms);
    match plugin_run(script, ctx_bytes, timeout_dur, &[]).await {
        Ok(Ok(output)) => {
            if let Err(message) =
                validate_plugin_output("plugin command", &output.stdout, &output.stderr)
            {
                return Outcome::err(message);
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Outcome::err(format!(
                    "plugin command '{}' handler exited with {}: {}",
                    config.name,
                    output.status,
                    stderr.trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                return Outcome::err(format!(
                    "plugin command '{}' handler returned empty stdout",
                    config.name
                ));
            }
            match serde_json::from_str::<Value>(&stdout) {
                Ok(v) if v.is_object() => {
                    let notify = v.get("notify").and_then(|n| n.as_str());
                    let status = if v.get("status").is_some() {
                        Some(v.get("status").and_then(|s| s.as_str()).unwrap_or(""))
                    } else {
                        None
                    };
                    emit_plugin_ui_side_effects(&config.plugin_name, notify, status);

                    if let Some(err) = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        Outcome::err(format!("plugin command '{}': {}", config.name, err))
                    } else {
                        let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(true);
                        let output_text = v
                            .get("output")
                            .or_else(|| v.get("result"))
                            .and_then(|o| o.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| stdout.clone());
                        if ok {
                            Outcome::ok(output_text)
                        } else {
                            Outcome::err(output_text)
                        }
                    }
                }
                Ok(_) => Outcome::ok(stdout),
                Err(_) => Outcome::ok(stdout),
            }
        }
        Ok(Err(e)) => Outcome::err(format!(
            "plugin command '{}' handler wait error: {e}",
            config.name
        )),
        Err(_) => Outcome::err(format!(
            "plugin command '{}' handler timed out after {}ms",
            config.name, config.timeout_ms
        )),
    }
}

/// Result of a `memory_provider` script invocation.
#[derive(Clone, Debug, Default)]
pub struct MemoryProviderResult {
    pub ok: bool,
    pub output: String,
    pub injection: String,
    pub id: String,
    pub entries: Vec<Value>,
}

impl MemoryProviderResult {
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: msg.into(),
            ..Default::default()
        }
    }

    /// Convert a write/list/forget result into a tool [`Outcome`].
    pub fn into_outcome(self) -> Outcome {
        if self.ok {
            Outcome::ok(self.output)
        } else {
            Outcome::err(self.output)
        }
    }
}

/// Invoke a plugin memory-provider script with the given `action` and `args`.
/// Used for standing-prompt injection, slash memory commands, compaction
/// extracts, and (when no tool overrides `memory`) the built-in memory tool.
pub async fn execute_memory_provider(
    config: &PluginMemoryProviderConfig,
    action: &str,
    args: &Value,
    workspace: &str,
    session_id: &str,
) -> MemoryProviderResult {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let ctx = json!({
        "action": action,
        "args": args,
        "workspace": workspace,
        "session_id": session_id,
        "timestamp": timestamp,
    });
    let ctx_bytes = serde_json::to_vec(&ctx).unwrap_or_default();
    if let Err(message) = validate_plugin_io("memory provider", ctx_bytes.len()) {
        return MemoryProviderResult::err(message);
    }

    let timeout_dur = Duration::from_millis(config.timeout_ms);
    match plugin_run(&config.script, ctx_bytes, timeout_dur, &[]).await {
        Ok(Ok(output)) => {
            if let Err(message) =
                validate_plugin_output("memory provider", &output.stdout, &output.stderr)
            {
                return MemoryProviderResult::err(message);
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return MemoryProviderResult::err(format!(
                    "memory_provider '{}' exited with {}: {}",
                    config.plugin_name,
                    output.status,
                    stderr.trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if stdout.is_empty() {
                return MemoryProviderResult::err(format!(
                    "memory_provider '{}' returned empty stdout",
                    config.plugin_name
                ));
            }
            parse_memory_provider_stdout(&stdout)
        }
        Ok(Err(e)) => MemoryProviderResult::err(format!(
            "memory_provider '{}' wait error: {e}",
            config.plugin_name
        )),
        Err(_) => MemoryProviderResult::err(format!(
            "memory_provider '{}' timed out after {}ms",
            config.plugin_name, config.timeout_ms
        )),
    }
}

/// Blocking variant for sync call sites (system-prompt build). Spawns a
/// dedicated thread with its own current-thread tokio runtime so we never
/// nest `block_on` on an existing runtime.
pub fn execute_memory_provider_blocking(
    config: &PluginMemoryProviderConfig,
    action: &str,
    args: &Value,
    workspace: &str,
    session_id: &str,
) -> MemoryProviderResult {
    let config = config.clone();
    let action = action.to_string();
    let args = args.clone();
    let workspace = workspace.to_string();
    let session_id = session_id.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(MemoryProviderResult::err(format!(
                    "memory_provider runtime: {e}"
                )));
                return;
            }
        };
        let result = rt.block_on(execute_memory_provider(
            &config,
            &action,
            &args,
            &workspace,
            &session_id,
        ));
        let _ = tx.send(result);
    });
    match rx.recv() {
        Ok(r) => {
            let _ = handle.join();
            r
        }
        Err(_) => {
            let _ = handle.join();
            MemoryProviderResult::err("memory_provider worker thread died")
        }
    }
}

/// Standing-prompt injection via the active memory provider. Soft-fails to
/// an empty string so a broken provider never blocks a turn.
pub fn memory_provider_inject(
    config: &PluginMemoryProviderConfig,
    workspace: &str,
    query: &str,
) -> String {
    let args = if query.is_empty() {
        json!({})
    } else {
        json!({ "query": query })
    };
    let result = execute_memory_provider_blocking(config, "inject", &args, workspace, "");
    if result.ok {
        result.injection
    } else {
        String::new()
    }
}

fn parse_memory_provider_stdout(stdout: &str) -> MemoryProviderResult {
    match serde_json::from_str::<Value>(stdout) {
        Ok(v) if v.is_object() => {
            if let Some(err) = v
                .get("error")
                .and_then(|e| e.as_str())
                .filter(|s| !s.is_empty())
            {
                return MemoryProviderResult::err(err.to_string());
            }
            let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(true);
            let output = v
                .get("output")
                .or_else(|| v.get("result"))
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            let injection = v
                .get("injection")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            let id = v
                .get("id")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            let entries = v
                .get("entries")
                .and_then(|e| e.as_array())
                .cloned()
                .unwrap_or_default();
            let output = if output.is_empty() && !ok {
                "memory_provider failed (action response had ok=false)".to_string()
            } else {
                output
            };
            MemoryProviderResult {
                ok,
                output,
                injection,
                id,
                entries,
            }
        }
        Ok(_) | Err(_) => MemoryProviderResult::err(format!(
            "memory_provider returned non-JSON stdout: {}",
            stdout.chars().take(200).collect::<String>()
        )),
    }
}

// ---- helpers ----

/// Return the default timeout for a hook point.
fn default_hook_timeout(hook_name: &str) -> u64 {
    hook_policy(hook_name).default_timeout_ms
}

/// Validate a script path declared by a plugin (hook or tool): reject `..`
/// escapes, require the canonicalized path to stay within the plugin's
/// canonical directory, and confirm it exists and is executable. Returns the
/// canonical script path on success. Used for plugin-declared tools; the hook
/// loader does the same checks inline (kept separate to avoid disturbing the
/// proven hook path + its exact error messages).
fn validate_plugin_script(canon_dir: &Path, script_rel: &str) -> Result<PathBuf, String> {
    let rel = Path::new(script_rel);
    {
        use std::path::Component;
        for comp in rel.components() {
            if let Component::ParentDir = comp {
                return Err(format!(
                    "script {:?} escapes the plugin directory",
                    script_rel
                ));
            }
        }
    }
    let abs = canon_dir.join(rel);
    let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
    if !canon.starts_with(canon_dir) {
        return Err(format!(
            "script {:?} escapes the plugin directory",
            script_rel
        ));
    }
    if !canon.exists() {
        return Err(format!("script {:?} does not exist", script_rel));
    }
    if !is_executable(&canon) {
        return Err(format!(
            "script {:?} is not executable (try chmod +x)",
            script_rel
        ));
    }
    Ok(canon)
}

/// Validate a prompt-backed command's template file: path-confined to the
/// plugin directory (no absolute paths, no `..`), exists, is a regular file,
/// and within the size limit. Returns the canonical absolute path. Unlike
/// `validate_plugin_script` the file need not be executable — it is read as
/// text and rendered, not spawned.
fn validate_prompt_file(canon_dir: &Path, prompt_rel: &str) -> Result<PathBuf, String> {
    let rel = Path::new(prompt_rel);
    if rel.is_absolute() {
        return Err(format!(
            "prompt_file {:?} must be relative to the plugin directory",
            prompt_rel
        ));
    }
    {
        use std::path::Component;
        for comp in rel.components() {
            if let Component::ParentDir = comp {
                return Err(format!(
                    "prompt_file {:?} escapes the plugin directory",
                    prompt_rel
                ));
            }
        }
    }
    let abs = canon_dir.join(rel);
    let canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
    if !canon.starts_with(canon_dir) {
        return Err(format!(
            "prompt_file {:?} escapes the plugin directory",
            prompt_rel
        ));
    }
    let meta = std::fs::metadata(&canon).map_err(|_| {
        format!(
            "prompt_file {:?} does not exist or is unreadable",
            prompt_rel
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "prompt_file {:?} is not a regular file",
            prompt_rel
        ));
    }
    if meta.len() as usize > MAX_PROMPT_COMMAND_BYTES {
        return Err(format!(
            "prompt_file {:?} exceeds the {} byte limit",
            prompt_rel, MAX_PROMPT_COMMAND_BYTES
        ));
    }
    Ok(canon)
}

/// Resolve a plugin manifest `oauth` block into a loaded [`PluginOauthConfig`]:
/// validates the provider_id/base_url/kind, resolves the token-file path under
/// `~/.config/catalyst-code/oauth/`, and resolves every declared script (shared
/// `script` default + per-action overrides) to an absolute, path-confined,
/// executable file. Token resolution is mandatory (a provider that can never
/// produce a token is useless); login/complete fall back to the shared script
/// and error at runtime only if neither exists.
fn load_oauth_entry(
    canon_dir: &Path,
    entry: OauthManifestEntry,
) -> Result<PluginOauthConfig, String> {
    let provider_id = entry.provider_id.clone();
    if provider_id.is_empty() {
        return Err("oauth provider_id is empty".into());
    }
    if entry.base_url.is_empty() {
        return Err(format!(
            "oauth provider '{provider_id}' has an empty base_url"
        ));
    }
    let kind = match entry.kind.as_deref().unwrap_or("openai") {
        "openai" => ProviderKind::OpenAI,
        "anthropic" => ProviderKind::Anthropic,
        other => {
            return Err(format!(
                "oauth provider '{provider_id}' has invalid kind '{other}' (use 'openai' or 'anthropic')"
            ))
        }
    };
    // Token files live directly under ~/.config/catalyst-code/oauth/. Treat
    // plugin manifests as untrusted input: accepting absolute paths, parent
    // traversal, or nested/symlinked parents would let logout delete arbitrary
    // host files.
    let token_dir = crate::config::home_dir()
        .map(|h| h.join(".config/catalyst-code/oauth"))
        .unwrap_or_else(|| PathBuf::from(".config/catalyst-code/oauth"));
    let token_name = entry
        .token_path
        .clone()
        .unwrap_or_else(|| format!("{provider_id}.json"));
    let token_component = Path::new(&token_name);
    let mut components = token_component.components();
    let valid_filename = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !valid_filename {
        return Err(format!(
            "oauth provider '{provider_id}' token_path must be one relative filename"
        ));
    }
    let token_path = token_dir.join(token_component);

    // Resolve the shared default (keyed "*") + per-action overrides.
    let mut scripts: HashMap<String, PathBuf> = HashMap::new();
    if let Some(s) = &entry.script {
        scripts.insert("*".to_string(), validate_plugin_script(canon_dir, s)?);
    }
    for (action, opt) in [
        ("login", &entry.login_script),
        ("complete", &entry.complete_script),
        ("token", &entry.token_script),
    ] {
        if let Some(s) = opt {
            scripts.insert(action.to_string(), validate_plugin_script(canon_dir, s)?);
        }
    }
    // Token resolution is essential — without it the provider can never
    // authenticate a turn.
    if scripts.get("token").or_else(|| scripts.get("*")).is_none() {
        return Err(format!(
            "oauth provider '{provider_id}' has no token script: set 'script' (handles all actions) or 'token_script'"
        ));
    }

    let label = entry.label.unwrap_or_else(|| provider_id.clone());
    // Validate declared env passthrough: names only, well-formed, and never
    // secret-looking — the whole point of env scrubbing is that plugin
    // scripts can't see API keys/tokens.
    for name in &entry.env_passthrough {
        let well_formed = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.chars().next().unwrap_or('0').is_ascii_digit();
        let upper = name.to_ascii_uppercase();
        let secret_like = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"]
            .iter()
            .any(|w| upper.contains(w));
        if !well_formed || secret_like {
            return Err(format!(
                "oauth provider '{provider_id}' env_passthrough entry '{name}' is invalid or secret-looking (KEY/TOKEN/SECRET/PASSWORD/CREDENTIAL names are not allowed)"
            ));
        }
    }
    let detect_path = entry
        .detect_path
        .as_deref()
        .map(resolve_oauth_detect_path)
        .transpose()?;
    Ok(PluginOauthConfig {
        provider_id,
        label,
        kind,
        base_url: entry.base_url,
        description: entry.description.unwrap_or_default(),
        headers: entry.headers,
        redirect_path: entry
            .redirect_path
            .unwrap_or_else(|| "/callback".to_string()),
        token_path,
        detect_path,
        scripts,
        login_timeout_ms: entry.login_timeout_ms.unwrap_or(120_000),
        token_timeout_ms: entry.token_timeout_ms.unwrap_or(30_000),
        env_passthrough: entry.env_passthrough,
    })
}

/// Resolve the small, intentionally constrained external-credential path
/// surface used by first-party OAuth bundles. `$CODEX_HOME/auth.json` follows
/// the official Codex CLI location and honors CODEX_HOME when it is set;
/// `~/.codex/auth.json` is accepted as the equivalent spelling. Other paths
/// are relative to the user's home and may not escape it with `..` or use an
/// absolute path.
fn resolve_oauth_detect_path(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("oauth detect_path must not be empty".into());
    }
    if raw == "$CODEX_HOME/auth.json" || raw == "~/.codex/auth.json" {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| crate::config::home_dir().map(|h| h.join(".codex")))
            .ok_or("oauth detect_path needs HOME or CODEX_HOME to be set")?;
        return Ok(codex_home.join("auth.json"));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!(
            "oauth detect_path '{raw}' must be relative, $CODEX_HOME/auth.json, or ~/.codex/auth.json"
        ));
    }
    for component in path.components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(format!("oauth detect_path '{raw}' may not contain '..'"));
        }
    }
    let home = crate::config::home_dir().ok_or("oauth detect_path needs HOME to be set")?;
    Ok(home.join(path))
}

/// Cross-platform check for whether a hook script is executable.
/// - Unix: any executable permission bit set (owner/group/other).
/// - Windows / non-Unix: no permission bit exists, so any file that exists
///   counts as executable (the OS governs launch by extension; a bad or
///   missing interpreter surfaces as a spawn error at hook execution time).
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.exists()
    }
}

/// Current unix time in seconds (0 on clock error). Used by the OAuth token
/// cache expiry check.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Normalize the plugin login flow name while keeping the manifest contract
/// forwards-compatible with providers that call this `auto` or `device_code`.
fn oauth_flow(resp: &Value, default: &str) -> String {
    resp.get("flow")
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .trim()
        .to_ascii_lowercase()
}

fn oauth_flow_is_auto(resp: &Value) -> bool {
    resp.get("auto_complete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || matches!(
            oauth_flow(resp, "").as_str(),
            "poll" | "auto" | "device_code" | "device-code"
        )
}

fn oauth_flow_is_existing(resp: &Value) -> bool {
    resp.get("ok").and_then(|v| v.as_bool()) != Some(false)
        && matches!(
            oauth_flow(resp, "").as_str(),
            "already_authenticated" | "already-authenticated" | "existing" | "authenticated"
        )
}

fn oauth_login_error(resp: &Value) -> Option<String> {
    if resp.get("ok").and_then(|v| v.as_bool()) != Some(false) {
        return None;
    }
    Some(
        resp.get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error")
            .to_string(),
    )
}

/// Parse optional `"headers": [["Name","value"], …]` from a plugin `token`
/// action response. Ignores malformed entries.
fn parse_oauth_token_headers(v: Option<&Value>) -> Vec<(String, String)> {
    let Some(arr) = v.and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in arr {
        if let Some(pair) = item.as_array() {
            if pair.len() >= 2 {
                if let (Some(k), Some(val)) = (pair[0].as_str(), pair[1].as_str()) {
                    if !k.is_empty() && !val.is_empty() {
                        out.push((k.to_string(), val.to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Extract the lowercased host from a URL (best-effort, no `url` crate dep).
/// `https://api.x.ai/v1` → `api.x.ai`. Used to match a manually-configured
/// provider to a plugin OAuth declaration by endpoint host.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, h)| h).unwrap_or(url);
    let host = rest.split(['/', ':']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Pick a Python interpreter, preferring `python3` then `python`, cached.
/// Falls back to `python3` (which will fail-to-spawn gracefully if truly
/// absent) so a missing interpreter never panics.
fn python_interpreter() -> String {
    use std::sync::OnceLock;
    static INTERP: OnceLock<String> = OnceLock::new();
    INTERP
        .get_or_init(|| {
            for cand in ["python3", "python"] {
                if let Ok(o) = std::process::Command::new(cand).arg("--version").output() {
                    if o.status.success() {
                        return cand.to_string();
                    }
                }
            }
            "python3".to_string()
        })
        .clone()
}

/// Build the command to run a hook script, selecting the right interpreter by
/// extension so plugins work cross-platform. On Unix a shebang handles `*.sh`;
/// on Windows `.bat`/`.cmd`/`.exe` launch directly, `.ps1` uses powershell,
/// Builds the `tokio::process::Command` for a plugin script: the interpreter is
/// selected by extension — `.ps1` uses PowerShell, `.py` uses python, and
/// `.sh`/`.bash` use `bash` (Git Bash/WSL) when present.
/// `CATALYST_CODE_SHELL` overrides the interpreter for `.sh`/`.bash`.
///
/// `extra_env` are `(name, value)` pairs the caller has already vetted (e.g. a
/// plugin OAuth provider's declared `env_passthrough`).
fn hook_command_with_env(script: &Path, extra_env: &[(String, String)]) -> Command {
    let ext = script
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let mut c = match ext.as_str() {
        "bat" | "cmd" | "exe" | "com" => Command::new(script),
        "ps1" => {
            // Prefer pwsh when available so hooks match the bash-tool default.
            let mut c = Command::new(crate::sandbox::policy::resolve_powershell_program());
            c.arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(script);
            c
        }
        "py" => {
            // Prefer `python3` (present on most Linux/macOS); fall back to
            // `python` (common on Windows / some distros). Launching a `.py`
            // hook as `python` on a python3-only system fails to spawn, and for
            // the advisory pre_turn hook that silently skips it — so vision
            // handoff would never run. Probe once and cache the interpreter.
            let mut c = Command::new(python_interpreter());
            c.arg(script);
            c
        }
        "sh" | "bash" => {
            // Prefer an explicit override, then bash (Git Bash/WSL on Windows).
            // On bare Windows without bash the spawn fails → graceful pre-hook deny.
            if let Ok(shell) = std::env::var("CATALYST_CODE_SHELL") {
                let mut c = Command::new(shell);
                c.arg(script);
                c
            } else {
                let mut c = Command::new("bash");
                c.arg(script);
                c
            }
        }
        _ => Command::new(script),
    };
    // Plugin/hook/oauth scripts are untrusted user/repo code: NEVER let them
    // inherit the parent's environment — that would leak every *_API_KEY,
    // UMANS_*, CATALYST_CODE_* the user exported. Clear the child env and
    // re-inject only the minimal set the interpreter + scripts need. On
    // Windows also re-inject the baseline process env PowerShell/cmd need
    // (SystemRoot/PATHEXT/COMSPEC/…) — without these, .ps1/.bat hooks fail to
    // start. All real context travels via the stdin JSON.
    c.env_clear();
    if let Ok(v) = std::env::var("PATH") {
        c.env("PATH", v);
    }
    if let Ok(v) = std::env::var("HOME") {
        c.env("HOME", v);
    }
    if let Ok(v) = std::env::var("TMPDIR") {
        c.env("TMPDIR", v);
    }
    if let Ok(v) = std::env::var("USER") {
        c.env("USER", v);
    }
    // Windows baseline required by PowerShell, cmd, and most .bat helpers.
    #[cfg(target_os = "windows")]
    {
        for key in [
            "SystemRoot",
            "windir",
            "PATHEXT",
            "COMSPEC",
            "TEMP",
            "TMP",
            "APPDATA",
            "LOCALAPPDATA",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "USERNAME",
            "USERDOMAIN",
            "NUMBER_OF_PROCESSORS",
            "PROCESSOR_ARCHITECTURE",
            "OS",
            "ProgramData",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "CommonProgramFiles",
            "PSModulePath",
        ] {
            if let Ok(v) = std::env::var(key) {
                c.env(key, v);
            }
        }
    }
    // Memory-provider plugins (Engraphis, etc.) need their DB/embed config and
    // optionally a custom PYTHONPATH. These are not API secrets.
    for key in [
        "ENGRAPHIS_DB_PATH",
        "ENGRAPHIS_EMBED_MODEL",
        "ENGRAPHIS_EMBED_DIM",
        "ENGRAPHIS_EXTRACTOR",
        "ENGRAPHIS_WORKSPACES",
        "PYTHONPATH",
    ] {
        if let Ok(v) = std::env::var(key) {
            c.env(key, v);
        }
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c
}

/// Resolve a plugin OAuth provider's declared (validated) `env_passthrough`
/// values from the current process env. Unset vars are skipped. Only names
/// the manifest declared — and `load_oauth_entry` vetted as non-secret —
/// ever reach the script.
fn oauth_script_env(cfg: &PluginOauthConfig) -> Vec<(String, String)> {
    cfg.env_passthrough
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
        .collect()
}

/// Sidecar written next to an installed plugin so a future auto-updater can
/// re-fetch the same GitHub Release source zip (or a newer tag).
const PLUGIN_SOURCE_META_FILE: &str = ".catalyst-plugin-source.json";

// Workspace installs are recorded in the user-owned trust store. No marker
// file is written into the project tree because a repository must not be able
// to self-authorize its own plugin hooks.
#[cfg(test)]
fn write_user_installed_marker(dest_dir: &Path) {
    let _ = std::fs::write(
        dest_dir.join(".catalyst-plugin-user-installed"),
        "test-only user install marker\n",
    );
}

/// A parsed GitHub plugin install source. Installs always go through a
/// **Release** source zip so the tag is a real version pin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubPluginSource {
    owner: String,
    repo: String,
    /// Specific release tag (`v1.2.0`). `None` → GitHub's `/releases/latest`.
    tag: Option<String>,
    /// Optional path inside the release zip where `plugin.json` lives
    /// (monorepo layout).
    subdir: Option<String>,
}

/// Persisted install provenance for GitHub Release installs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PluginSourceMeta {
    kind: String,
    owner: String,
    repo: String,
    /// Release tag that was installed (e.g. `v1.2.0`).
    tag: String,
    /// Canonical repo URL (`https://github.com/owner/repo`).
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subdir: Option<String>,
    /// Unix seconds when this install/update wrote the sidecar.
    installed_at: u64,
}

/// True when `source` looks like a GitHub repo URL or `owner/repo` shorthand
/// (as opposed to a local filesystem path).
fn looks_like_github_source(source: &str) -> bool {
    let s = source.trim();
    if s.contains("github.com") || s.starts_with("git@github.com:") {
        return true;
    }
    // owner/repo or owner/repo@tag — but not an absolute/relative path.
    if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') {
        return false;
    }
    if s.contains('\\') {
        return false;
    }
    let base = s.split(['@', '#', ':']).next().unwrap_or(s);
    let mut parts = base.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(o), Some(r), None) if !o.is_empty() && !r.is_empty()
            && o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
            && r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    )
}

/// Parse a GitHub plugin source string into owner/repo/tag/subdir.
///
/// Accepted forms:
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo@v1.2.0`
/// - `https://github.com/owner/repo#v1.2.0`
/// - `https://github.com/owner/repo/releases/tag/v1.2.0`
/// - `https://github.com/owner/repo/tree/v1.2.0/path/to/plugin`
/// - `git@github.com:owner/repo.git`
/// - `owner/repo` / `owner/repo@v1.2.0` / `owner/repo@v1.2.0:subdir`
/// - `owner/repo:subdir` (latest release, monorepo subdir)
fn parse_github_plugin_source(source: &str) -> Option<GithubPluginSource> {
    if !looks_like_github_source(source) {
        return None;
    }
    let s = source.trim();

    // Normalize git@ and strip trailing .git early where possible.
    let s = if let Some(rest) = s.strip_prefix("git@github.com:") {
        format!("https://github.com/{}", rest.trim_end_matches(".git"))
    } else {
        s.to_string()
    };
    let s = s.replace("https://www.github.com/", "https://github.com/");
    let s = if s.starts_with("http://github.com/") {
        format!("https://{}", s.trim_start_matches("http://"))
    } else {
        s
    };

    // Split off @tag or #tag, and optional :subdir after the tag.
    let (main, tag_and_subdir) = if let Some(i) = s.rfind(['@', '#']) {
        let (a, b) = s.split_at(i);
        (a.to_string(), Some(b[1..].to_string()))
    } else {
        (s.clone(), None)
    };

    let (tag_from_suffix, subdir_from_suffix) = match tag_and_subdir {
        Some(rest) => {
            if let Some((tag, sub)) = rest.split_once(':') {
                (
                    Some(tag.trim().to_string()).filter(|t| !t.is_empty()),
                    Some(sub.trim().trim_matches('/').to_string()).filter(|p| !p.is_empty()),
                )
            } else {
                (
                    Some(rest.trim().to_string()).filter(|t| !t.is_empty()),
                    None,
                )
            }
        }
        None => (None, None),
    };

    // Shorthand monorepo form without a tag: owner/repo:subdir
    let (main, subdir_from_shorthand) = if tag_from_suffix.is_none()
        && subdir_from_suffix.is_none()
        && !main.starts_with("https://")
    {
        if let Some((repo_part, sub)) = main.split_once(':') {
            (
                repo_part.to_string(),
                Some(sub.trim().trim_matches('/').to_string()).filter(|p| !p.is_empty()),
            )
        } else {
            (main, None)
        }
    } else {
        (main, None)
    };

    let path = if let Some(rest) = main.strip_prefix("https://github.com/") {
        rest.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_string()
    } else if !main.contains("://") && !main.starts_with("git@") {
        main.trim_end_matches('/')
            .trim_end_matches(".git")
            .to_string()
    } else {
        return None;
    };

    let segments: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[0].to_string();
    let repo = segments[1].to_string();

    // Path forms: /releases/tag/TAG or /tree/REF[/subdir...]
    let (tag_from_path, subdir_from_path) =
        if segments.len() >= 4 && segments[2] == "releases" && segments[3] == "tag" {
            (
                Some(segments[4..].join("/")).filter(|t| !t.is_empty()),
                None,
            )
        } else if segments.len() >= 4 && segments[2] == "tree" {
            let tag = segments[3].to_string();
            let sub = if segments.len() > 4 {
                Some(segments[4..].join("/"))
            } else {
                None
            };
            (Some(tag), sub)
        } else if segments.len() > 2 {
            // Unknown extra path — not a supported GitHub plugin URL.
            return None;
        } else {
            (None, None)
        };

    let tag = tag_from_suffix.or(tag_from_path);
    let subdir = subdir_from_suffix
        .or(subdir_from_shorthand)
        .or(subdir_from_path);

    Some(GithubPluginSource {
        owner,
        repo,
        tag,
        subdir,
    })
}

/// Fetch a GitHub Release source zip, extract it, and locate the plugin root.
/// Returns `(plugin_dir, source_meta, tmp_root_to_cleanup)`.
async fn fetch_github_release_plugin(
    src: &GithubPluginSource,
) -> Result<(PathBuf, PluginSourceMeta, PathBuf), String> {
    let release = resolve_github_release(src).await?;
    let bytes = download_url_bytes(&release.zipball_url, "release source zip").await?;

    let tmp_root = std::env::temp_dir().join(format!(
        "catalyst-plugin-{}-{}-{}",
        src.owner,
        src.repo,
        std::process::id()
    ));
    if tmp_root.exists() {
        let _ = std::fs::remove_dir_all(&tmp_root);
    }
    std::fs::create_dir_all(&tmp_root)
        .map_err(|e| format!("cannot create temp dir {}: {e}", tmp_root.display()))?;

    let extract_dir = tmp_root.join("extract");
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("cannot create extract dir: {e}"))?;
    extract_zip_bytes(&bytes, &extract_dir)?;

    let repo_root = find_single_top_level_dir(&extract_dir)?;
    let plugin_dir = resolve_plugin_dir(&repo_root, src.subdir.as_deref())?;
    // Release zips sometimes drop the executable bit; ensure hook/tool scripts
    // are runnable before validation.
    ensure_plugin_scripts_executable(&plugin_dir);

    let meta = PluginSourceMeta {
        kind: "github_release".into(),
        owner: src.owner.clone(),
        repo: src.repo.clone(),
        tag: release.tag,
        url: format!("https://github.com/{}/{}", src.owner, src.repo),
        subdir: src.subdir.clone(),
        installed_at: now_secs(),
    };
    Ok((plugin_dir, meta, tmp_root))
}

#[derive(Debug, Deserialize)]
struct GithubReleaseResponse {
    tag_name: String,
    zipball_url: String,
}

struct ResolvedRelease {
    tag: String,
    zipball_url: String,
}

async fn resolve_github_release(src: &GithubPluginSource) -> Result<ResolvedRelease, String> {
    let api = match &src.tag {
        Some(tag) => format!(
            "https://api.github.com/repos/{}/{}/releases/tags/{}",
            src.owner,
            src.repo,
            tag.trim_start_matches('/')
        ),
        None => format!(
            "https://api.github.com/repos/{}/{}/releases/latest",
            src.owner, src.repo
        ),
    };

    let client = github_http_client()?;
    let mut req = client
        .get(&api)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = github_token() {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("GitHub API request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("GitHub API read failed: {e}"))?;

    if status.as_u16() == 404 {
        return Err(match &src.tag {
            Some(tag) => format!(
                "no GitHub Release '{tag}' for {}/{}; publish a Release (recommended for versioning and updates) or check the tag",
                src.owner, src.repo
            ),
            None => format!(
                "no GitHub Releases found for {}/{}; publish a Release so the install can download a versioned source .zip (required for URL installs and future auto-updates)",
                src.owner, src.repo
            ),
        });
    }
    if !status.is_success() {
        return Err(format!(
            "GitHub API {} for {}/{}: {}",
            status,
            src.owner,
            src.repo,
            truncate_err(&body, 200)
        ));
    }

    let release: GithubReleaseResponse = serde_json::from_str(&body).map_err(|e| {
        format!(
            "GitHub release JSON parse error: {e} ({})",
            truncate_err(&body, 120)
        )
    })?;
    if release.zipball_url.is_empty() {
        return Err(format!(
            "GitHub release '{}' for {}/{} has no zipball_url",
            release.tag_name, src.owner, src.repo
        ));
    }
    Ok(ResolvedRelease {
        tag: release.tag_name,
        zipball_url: release.zipball_url,
    })
}

fn github_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("catalyst-code-plugin-install/0.2")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

async fn download_url_bytes(url: &str, label: &str) -> Result<Vec<u8>, String> {
    let client = github_http_client()?;
    let mut req = client.get(url);
    if let Some(token) = github_token() {
        req = req.bearer_auth(token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("download {label} failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download {label} failed: HTTP {}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("download {label} read failed: {e}"))?;
    // Guard against accidental huge downloads (plugin source zips should be small).
    const MAX_ZIP_BYTES: usize = 64 * 1024 * 1024;
    if bytes.len() > MAX_ZIP_BYTES {
        return Err(format!(
            "{label} is too large ({} bytes; max {} bytes)",
            bytes.len(),
            MAX_ZIP_BYTES
        ));
    }
    Ok(bytes.to_vec())
}

fn extract_zip_bytes(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let reader = Cursor::new(bytes);
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("invalid zip archive: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;
        let name = file
            .enclosed_name()
            .ok_or_else(|| format!("zip entry '{}' has an unsafe path", file.name()))?
            .to_path_buf();
        let out = dest.join(&name);

        if file.is_dir() || file.name().ends_with('/') {
            std::fs::create_dir_all(&out).map_err(|e| format!("mkdir {}: {e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let mut outfile =
            std::fs::File::create(&out).map_err(|e| format!("create {}: {e}", out.display()))?;
        std::io::copy(&mut file, &mut outfile)
            .map_err(|e| format!("extract {}: {e}", out.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = file.unix_mode() {
                let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

/// GitHub zipballs contain a single top-level directory (`owner-repo-<sha>/`).
fn find_single_top_level_dir(extract_dir: &Path) -> Result<PathBuf, String> {
    let mut dirs = Vec::new();
    let rd = std::fs::read_dir(extract_dir)
        .map_err(|e| format!("read extract dir {}: {e}", extract_dir.display()))?;
    for entry in rd {
        let entry = entry.map_err(|e| format!("extract dir entry: {e}"))?;
        let ft = entry
            .file_type()
            .map_err(|e| format!("extract dir file_type: {e}"))?;
        if ft.is_dir() {
            dirs.push(entry.path());
        }
    }
    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        0 => Err("release zip is empty".into()),
        _ => {
            // Unusual, but treat extract_dir itself as the root.
            Ok(extract_dir.to_path_buf())
        }
    }
}

/// Locate the directory that contains `plugin.json` inside an extracted release.
fn resolve_plugin_dir(repo_root: &Path, subdir: Option<&str>) -> Result<PathBuf, String> {
    if let Some(sub) = subdir {
        let rel = Path::new(sub);
        for comp in rel.components() {
            if matches!(comp, std::path::Component::ParentDir) {
                return Err(format!("plugin subdir '{sub}' must not contain '..'"));
            }
        }
        let dir = repo_root.join(rel);
        if !dir.join("plugin.json").is_file() {
            return Err(format!(
                "no plugin.json at subdir '{}' inside the release (looked in {})",
                sub,
                dir.display()
            ));
        }
        return Ok(dir);
    }

    if repo_root.join("plugin.json").is_file() {
        return Ok(repo_root.to_path_buf());
    }

    // Convenience: if exactly one immediate child has plugin.json, use it.
    let mut matches = Vec::new();
    if let Ok(rd) = std::fs::read_dir(repo_root) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() && p.join("plugin.json").is_file() {
                matches.push(p);
            }
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(format!(
            "no plugin.json found in the release root {}; pass owner/repo@tag:subdir for monorepos",
            repo_root.display()
        )),
        n => Err(format!(
            "found {n} plugin.json directories in the release; pass owner/repo@tag:subdir to pick one"
        )),
    }
}

fn write_plugin_source_meta(plugin_dir: &Path, meta: &PluginSourceMeta) -> Result<(), String> {
    let path = plugin_dir.join(PLUGIN_SOURCE_META_FILE);
    let raw = serde_json::to_string_pretty(meta)
        .map_err(|e| format!("serialize plugin source meta: {e}"))?;
    std::fs::write(&path, raw + "\n").map_err(|e| format!("write {}: {e}", path.display()))
}

/// Best-effort: mark scripts under `hooks/`, `tools/`, and `oauth/` executable
/// after a zip extract (GitHub zipballs usually preserve mode, but not always).
fn ensure_plugin_scripts_executable(plugin_dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for sub in ["hooks", "tools", "oauth"] {
            let dir = plugin_dir.join(sub);
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                let mut perms = meta.permissions();
                let mode = perms.mode();
                if mode & 0o111 == 0 {
                    perms.set_mode(mode | 0o755);
                    let _ = std::fs::set_permissions(&path, perms);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = plugin_dir;
    }
}

fn truncate_err(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.len() <= max {
        t.to_string()
    } else {
        format!("{}…", &t[..max])
    }
}

/// Directory / file names skipped when copying a plugin into the managed
/// plugins dir. Keeps local-path installs from dragging in venvs, git
/// metadata, caches, etc. (and avoids failing on venv `lib64 -> lib` symlinks).
const PLUGIN_COPY_SKIP: &[&str] = &[
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".runtime",
    "node_modules",
    ".DS_Store",
    ".tox",
    "dist",
    "build",
    ".eggs",
];

/// Recursively copy a directory from `src` to `dst`.
/// Skips junk dirs (see [`PLUGIN_COPY_SKIP`]). Symlinks are recreated when
/// possible; broken / unsupported link types are skipped with a warning rather
/// than failing the whole install.
fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {:?}: {e}", dst))?;

    let rd = std::fs::read_dir(src).map_err(|e| format!("read_dir {:?}: {e}", src))?;

    for entry in rd {
        let entry = entry.map_err(|e| format!("dir entry error: {e}"))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Never copy local dotenv credentials into the managed plugin
        // directory. Keep the checked-in example so plugins can still
        // document optional development configuration.
        let dotenv_secret =
            name_str == ".env" || (name_str.starts_with(".env.") && name_str != ".env.example");
        if dotenv_secret || PLUGIN_COPY_SKIP.iter().any(|s| *s == name_str) {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| format!("file_type error: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(&name);

        if ft.is_symlink() {
            match std::fs::read_link(&src_path) {
                Ok(target) => {
                    #[cfg(unix)]
                    {
                        if let Err(e) = std::os::unix::fs::symlink(&target, &dst_path) {
                            eprintln!("[plugins] skip symlink {:?} -> {:?}: {e}", src_path, target);
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        // Best-effort: if the link points at a regular file, copy it.
                        let resolved = src_path.parent().unwrap_or(src).join(&target);
                        if resolved.is_file() {
                            let _ = std::fs::copy(&resolved, &dst_path);
                        } else {
                            eprintln!("[plugins] skip symlink {:?} (non-unix)", src_path);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[plugins] skip unreadable symlink {:?}: {e}", src_path);
                }
            }
        } else if ft.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else if ft.is_file() {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {:?} -> {:?}: {e}", src_path, dst_path))?;
        } else {
            eprintln!("[plugins] skip non-file/dir {:?}", src_path);
        }
    }
    Ok(())
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {
        // No executable bit on Windows; hooks launch by extension.
    }

    /// Create a temporary directory that is cleaned up on drop.
    struct TmpDir {
        path: PathBuf,
    }

    impl TmpDir {
        fn new(prefix: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("catalyst_code_plugin_test_{}_{}", prefix, n));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TmpDir { path }
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Write a minimal executable shell script that outputs the given JSON.
    fn write_hook_script(dir: &Path, name: &str, stdout_json: &str, exit_code: u32) -> PathBuf {
        let script = dir.join(name);
        let content = format!("#!/bin/sh\necho '{}'\nexit {}\n", stdout_json, exit_code);
        fs::write(&script, &content).unwrap();
        make_executable(&script);
        script
    }

    /// Write a complete plugin to a directory.
    fn write_plugin(dir: &Path, name: &str, version: &str, hooks_json: &str) {
        let manifest = format!(
            r#"{{
  "name": "{}",
  "version": "{}",
  "description": "Test plugin",
  "hooks": {}
}}"#,
            name, version, hooks_json
        );
        fs::write(dir.join("plugin.json"), manifest).unwrap();
    }

    // ---- manifest loading ----

    #[test]
    fn load_minimal_plugin() {
        let tmp = TmpDir::new("load_minimal");
        write_plugin(&tmp.path, "minimal", "1.0.0", "{}");
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(plugin.name, "minimal");
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.protocol_version, 1);
        assert!(plugin.capabilities.is_empty());
        assert_eq!(plugin.hooks.len(), 0);
        assert!(plugin.commands.is_empty());
        assert!(plugin.enabled);
    }

    #[test]
    fn future_plugin_protocol_is_rejected() {
        let tmp = TmpDir::new("future_protocol");
        fs::write(
            tmp.path.join("plugin.json"),
            r#"{"name":"future","version":"1","protocol_version":99}"#,
        )
        .unwrap();
        assert!(PluginManager::load_plugin_from_dir(&tmp.path)
            .unwrap_err()
            .contains("newer than supported"));
    }

    #[test]
    fn explicit_capabilities_must_cover_registered_features() {
        let tmp = TmpDir::new("capability_denied");
        let plugin_dir = write_plugin_with_tool(&tmp.path, "cap_tool", "");
        let manifest_path = plugin_dir.join("plugin.json");
        let manifest = fs::read_to_string(&manifest_path).unwrap().replacen(
            "\"version\": \"1.0.0\",",
            "\"version\": \"1.0.0\",\n  \"protocol_version\": 1,\n  \"capabilities\": [\"execute_subprocess\"],",
            1,
        );
        fs::write(&manifest_path, manifest).unwrap();
        let error = PluginManager::load_plugin_from_dir(&plugin_dir).unwrap_err();
        assert!(error.contains("missing required capabilities"));
        assert!(error.contains("register_tools"));
        assert!(error.contains("write_workspace"));
    }

    #[test]
    fn plugin_io_limits_reject_oversized_payloads() {
        assert!(validate_plugin_io("test", MAX_PLUGIN_INPUT_BYTES + 1).is_err());
        assert!(
            validate_plugin_output("test", &vec![0; MAX_PLUGIN_OUTPUT_BYTES + 1], &[]).is_err()
        );
    }

    #[test]
    fn plugin_copy_excludes_dotenv_secrets_but_keeps_example() {
        let src = TmpDir::new("copy_dotenv_src");
        let dst = TmpDir::new("copy_dotenv_dst");
        fs::write(src.path.join("plugin.json"), "{}").unwrap();
        fs::write(src.path.join(".env"), "SECRET=one\n").unwrap();
        fs::write(src.path.join(".env.production"), "SECRET=two\n").unwrap();
        fs::write(src.path.join(".env.example"), "SECRET=replace_me\n").unwrap();
        fs::create_dir_all(src.path.join(".runtime")).unwrap();
        fs::write(
            src.path.join(".runtime/private.log"),
            "secret runtime state\n",
        )
        .unwrap();

        copy_dir(&src.path, &dst.path).unwrap();

        assert!(!dst.path.join(".env").exists());
        assert!(!dst.path.join(".env.production").exists());
        assert!(dst.path.join(".env.example").exists());
        assert!(!dst.path.join(".runtime").exists());
    }

    #[test]
    fn load_plugin_with_commands() {
        let tmp = TmpDir::new("load_with_commands");
        let scripts = tmp.path.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        let script = write_hook_script(&scripts, "greet.sh", r#"{"ok":true,"output":"hi"}"#, 0);

        write_manifest(
            &tmp.path,
            r#"{
  "name": "cmd-plugin",
  "version": "1.0.0",
  "commands": [
    { "name": "greet", "description": "Say hello", "script": "scripts/greet.sh", "timeout_ms": 12000 },
    { "name": "", "script": "scripts/greet.sh" },
    { "name": "/ping", "description": "Ping", "script": "scripts/greet.sh" }
  ]
}"#,
        );

        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(plugin.commands.len(), 2);

        let greet = plugin.commands.iter().find(|c| c.name == "greet").unwrap();
        assert_eq!(greet.description, "Say hello");
        assert_eq!(greet.timeout_ms, 12_000);
        assert_eq!(greet.plugin_name, "cmd-plugin");
        assert_eq!(greet.mode, CommandMode::Script);
        assert_eq!(
            greet.script.as_ref().unwrap(),
            &std::fs::canonicalize(&script).unwrap()
        );

        let ping = plugin.commands.iter().find(|c| c.name == "ping").unwrap();
        assert_eq!(ping.timeout_ms, DEFAULT_POST_TIMEOUT_MS);
        assert_eq!(ping.plugin_name, "cmd-plugin");
    }

    #[test]
    fn load_rejects_reserved_command_name() {
        let tmp = TmpDir::new("load_reserved_cmd");
        let scripts = tmp.path.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        write_hook_script(&scripts, "x.sh", r#"{"ok":true}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{"name":"bad-cmd","version":"1.0.0","commands":[{"name":"help","script":"scripts/x.sh"}]}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("reserved"), "err={err}");
    }

    #[test]
    fn load_plugin_with_hooks() {
        let tmp = TmpDir::new("load_with_hooks");
        let hooks_dir = tmp.path.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let pre_script = write_hook_script(&hooks_dir, "pre_write.sh", r#"{"allow":true}"#, 0);
        let post_script = write_hook_script(&hooks_dir, "post_bash.sh", r#"{"allow":true}"#, 0);

        // Use relative paths in the manifest.
        write_plugin(
            &tmp.path,
            "with-hooks",
            "0.2.0",
            r#"{
          "pre_write": { "script": "hooks/pre_write.sh", "timeout_ms": 7000, "pass_args": true },
          "post_bash": { "script": "hooks/post_bash.sh" }
        }"#,
        );

        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(plugin.hooks.len(), 2);

        let pre = plugin.hooks.get("pre_write").unwrap();
        assert_eq!(pre.script, std::fs::canonicalize(&pre_script).unwrap());
        assert_eq!(pre.timeout_ms, 7000);
        assert!(pre.pass_args);

        let post = plugin.hooks.get("post_bash").unwrap();
        assert_eq!(post.script, std::fs::canonicalize(&post_script).unwrap());
        assert_eq!(post.timeout_ms, DEFAULT_POST_TIMEOUT_MS);
        assert!(!post.pass_args);
    }

    #[test]
    fn load_rejects_missing_script() {
        let tmp = TmpDir::new("load_missing_script");
        write_plugin(
            &tmp.path,
            "bad",
            "1.0.0",
            r#"{"pre_write": {"script": "hooks/nonexistent.sh"}}"#,
        );
        let result = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn load_rejects_non_executable() {
        let tmp = TmpDir::new("load_not_exe");
        let hooks_dir = tmp.path.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let script = hooks_dir.join("hook.sh");
        fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        // Leave without +x.
        write_plugin(
            &tmp.path,
            "not-exe",
            "1.0.0",
            r#"{"pre_write": {"script": "hooks/hook.sh"}}"#,
        );
        let result = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not executable"));
    }

    #[test]
    fn load_rejects_script_escape() {
        let tmp = TmpDir::new("load_escape");
        write_plugin(
            &tmp.path,
            "escape-artist",
            "1.0.0",
            r#"{"pre_write": {"script": "../hooks/outside.sh"}}"#,
        );
        let result = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("escapes"));
    }

    #[test]
    fn load_skips_unknown_hook() {
        let tmp = TmpDir::new("load_unknown_hook");
        let hooks_dir = tmp.path.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_hook_script(&hooks_dir, "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &tmp.path,
            "weird",
            "1.0.0",
            r#"{"pre_launch_missiles": {"script": "hooks/hook.sh"}}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert!(plugin.hooks.is_empty()); // unknown hook skipped
    }

    #[test]
    fn load_rejects_bad_json() {
        let tmp = TmpDir::new("load_bad_json");
        fs::write(tmp.path.join("plugin.json"), "not valid {{{").unwrap();
        let result = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(result.is_err());
    }

    #[test]
    fn load_rejects_empty_name() {
        let tmp = TmpDir::new("load_empty_name");
        write_plugin(&tmp.path, "", "1.0.0", "{}");
        let result = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(result.is_err());
    }

    // ---- PluginManager lifecycle ----

    #[test]
    fn manager_loads_plugins_on_new() {
        let tmp = TmpDir::new("mgr_loads");
        let plugin_dir = tmp.path.join("test-plugin");
        fs::create_dir_all(plugin_dir.join("hooks")).unwrap();
        write_hook_script(&plugin_dir.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &plugin_dir,
            "test-plugin",
            "1.0.0",
            r#"{"pre_write": {"script": "hooks/hook.sh"}}"#,
        );

        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__pm_test_ws__"), true);
        let plugins = mgr.list();
        assert_eq!(plugins.len(), 1);
        assert!(plugins.contains_key("test-plugin"));
    }

    #[test]
    fn project_plugins_skipped_unless_trusted() {
        // A plugin shipped inside the workspace (project-scoped) must be skipped
        // when trust_project is false — a repo must not auto-run its own hooks.
        let tmp = TmpDir::new("proj_skip");
        // workspace == the tmp dir; plugin under <tmp>/.catalyst-code/plugins/x
        let plugins_dir = tmp.path.join(".catalyst-code/plugins");
        let plugin_dir = plugins_dir.join("shady");
        fs::create_dir_all(plugin_dir.join("hooks")).unwrap();
        write_hook_script(&plugin_dir.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &plugin_dir,
            "shady",
            "1.0.0",
            r#"{"pre_bash":{"script":"hooks/hook.sh"}}"#,
        );
        let mgr = PluginManager::new(plugins_dir.clone(), tmp.path.clone(), false);
        assert!(
            mgr.list().is_empty(),
            "project plugin should be skipped when trust=false"
        );
        let skipped = mgr.skipped_project_plugins();
        assert_eq!(skipped, vec!["shady".to_string()]);

        // Same plugin loads when trust_project is true.
        let mgr2 = PluginManager::new(plugins_dir, tmp.path.clone(), true);
        assert_eq!(mgr2.list().len(), 1);
        assert!(mgr2.skipped_project_plugins().is_empty());
    }

    #[test]
    fn trust_decisions_gate_project_plugins() {
        // Undecided project plugins are skipped and reported as pending;
        // a "trust" decision loads them, a "deny" keeps them gated and the
        // decision is recorded for the prompt.
        let tmp = TmpDir::new("trust_gate");
        let plugins_dir = tmp.path.join(".catalyst-code/plugins");
        for (name, version) in [("shady", "1.0.0"), ("linter", "2.1.0")] {
            let plugin_dir = plugins_dir.join(name);
            fs::create_dir_all(plugin_dir.join("hooks")).unwrap();
            write_hook_script(&plugin_dir.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
            write_plugin(
                &plugin_dir,
                name,
                version,
                r#"{"pre_bash":{"script":"hooks/hook.sh"}}"#,
            );
        }

        // Undecided: both skipped, both pending, nothing loaded.
        let mgr = PluginManager::new(plugins_dir.clone(), tmp.path.clone(), false);
        assert!(mgr.list().is_empty());
        let pending = mgr.pending_trust_plugins();
        let names: Vec<String> = pending.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["linter".to_string(), "shady".to_string()]);
        assert!(pending.iter().all(|p| p.decision.is_none()));
        assert_eq!(mgr.skipped_project_plugins().len(), 2);

        // Trust "shady": it loads immediately; "linter" stays gated + pending.
        let mut decisions = HashMap::new();
        decisions.insert("shady".to_string(), "trust".to_string());
        mgr.apply_trust_decisions(decisions).unwrap();
        assert!(mgr.list().contains_key("shady"));
        assert!(!mgr.list().contains_key("linter"));
        let pending = mgr.pending_trust_plugins();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].name, "linter");

        // Deny "linter": now nothing is pending and nothing new loads.
        let mut decisions = HashMap::new();
        decisions.insert("linter".to_string(), "deny".to_string());
        mgr.apply_trust_decisions(decisions).unwrap();
        assert!(mgr.pending_trust_plugins().is_empty());
        assert!(!mgr.list().contains_key("linter"));
        assert!(mgr.list().contains_key("shady"));
        // Trust prompt JSON still lists denied plugins and trusted loaded
        // plugins so either decision can be changed later.
        let prompt = mgr.trust_prompt_json();
        assert_eq!(prompt.len(), 2);
        let linter = prompt.iter().find(|p| p["name"] == "linter").unwrap();
        assert_eq!(linter["decision"], "deny");
        let shady = prompt.iter().find(|p| p["name"] == "shady").unwrap();
        assert_eq!(shady["decision"], "trust");
        // Re-deciding linter to trust loads it.
        let mut decisions = HashMap::new();
        decisions.insert("linter".to_string(), "trust".to_string());
        mgr.apply_trust_decisions(decisions).unwrap();
        assert!(mgr.list().contains_key("linter"));
        assert!(mgr.pending_trust_plugins().is_empty());
    }

    #[test]
    fn global_user_plugins_load_alongside_project() {
        // A plugin in the user (global) plugins dir loads regardless of
        // trust_project (it lives outside the workspace), and a same-named
        // project plugin overrides it.
        let ws = TmpDir::new("glob_ws");
        let global = TmpDir::new("glob_user_plugins");
        let proj_plugins = ws.path.join(".catalyst-code/plugins");

        // Global plugin "vision-fake" (outside the workspace).
        let gdir = global.path.join("vision-fake");
        fs::create_dir_all(gdir.join("hooks")).unwrap();
        write_hook_script(&gdir.join("hooks"), "h.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &gdir,
            "vision-fake",
            "1.0.0",
            r#"{"pre_turn":{"script":"hooks/h.sh"}}"#,
        );

        // trust_project=false, no project plugins present: global loads, none skipped.
        let mgr = PluginManager::new_with_user_plugins_dir(
            proj_plugins.clone(),
            Some(global.path.clone()),
            ws.path.clone(),
            false,
        );
        assert!(mgr.list().contains_key("vision-fake"));
        assert!(mgr.skipped_project_plugins().is_empty());

        // Add a same-named project plugin inside the workspace.
        let pdir = proj_plugins.join("vision-fake");
        fs::create_dir_all(pdir.join("hooks")).unwrap();
        write_hook_script(&pdir.join("hooks"), "h.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &pdir,
            "vision-fake",
            "9.9.9",
            r#"{"pre_turn":{"script":"hooks/h.sh"}}"#,
        );

        // trust_project=true: project plugin loads and OVERRIDES the global one.
        let mgr2 = PluginManager::new_with_user_plugins_dir(
            proj_plugins.clone(),
            Some(global.path.clone()),
            ws.path.clone(),
            true,
        );
        assert_eq!(mgr2.list().get("vision-fake").unwrap().version, "9.9.9");

        // trust_project=false: the project plugin is skipped (recorded), and
        // the global one still loads.
        let mgr3 = PluginManager::new_with_user_plugins_dir(
            proj_plugins,
            Some(global.path.clone()),
            ws.path.clone(),
            false,
        );
        assert_eq!(mgr3.list().get("vision-fake").unwrap().version, "1.0.0");
        assert_eq!(
            mgr3.skipped_project_plugins(),
            vec!["vision-fake".to_string()]
        );
    }

    #[test]
    fn explicit_global_directory_is_trusted_when_nested_in_workspace() {
        let ws = TmpDir::new("nested_global_ws");
        let global = ws.path.join(".catalyst-code/global-plugins");
        let project = ws.path.join(".catalyst-code/plugins");
        let pdir = global.join("codex");
        fs::create_dir_all(&pdir).unwrap();
        write_manifest(
            &pdir,
            r#"{"name":"codex","version":"0.1.0","description":"first-party"}"#,
        );

        let mgr =
            PluginManager::new_with_user_plugins_dir(project, Some(global), ws.path.clone(), false);
        assert!(mgr.list().contains_key("codex"));
        assert!(mgr.skipped_project_plugins().is_empty());
    }

    #[test]
    fn install_and_remove_plugin() {
        let tmp = TmpDir::new("mgr_install");
        let mgr = PluginManager::new(
            tmp.path.join("managed"),
            PathBuf::from("/__pm_test_ws__"),
            true,
        );

        // Create a plugin source dir. (install target is outside the test
        // workspace dummy, so it loads regardless of trust_project.)
        let src = TmpDir::new("install_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "fresh",
            "2.0.0",
            r#"{"post_write": {"script": "hooks/hook.sh"}}"#,
        );

        let installed = mgr
            .install(&src.path, PluginInstallScope::Workspace)
            .unwrap();
        assert_eq!(installed.name, "fresh");
        assert_eq!(installed.version, "2.0.0");

        // Check that it was copied into the managed dir.
        assert!(mgr.list().contains_key("fresh"));
        assert!(tmp.path.join("managed/fresh/plugin.json").exists());

        // Remove it.
        mgr.remove("fresh").unwrap();
        assert!(mgr.list().is_empty());
        assert!(!tmp.path.join("managed/fresh").exists());
    }

    #[test]
    fn install_respects_global_vs_workspace_scope() {
        let tmp = TmpDir::new("mgr_scope");
        let ws = tmp.path.join("workspace");
        let project_plugins = ws.join(".catalyst-code/plugins");
        let global_plugins = tmp.path.join("global-plugins");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&project_plugins).unwrap();
        fs::create_dir_all(&global_plugins).unwrap();

        let mgr = PluginManager::new_with_user_plugins_dir(
            project_plugins.clone(),
            Some(global_plugins.clone()),
            ws.clone(),
            false, // trust off — workspace user-installs still load via marker
        );

        let src = TmpDir::new("scope_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "scoped",
            "1.0.0",
            r#"{"post_write": {"script": "hooks/hook.sh"}}"#,
        );

        let global = mgr.install(&src.path, PluginInstallScope::Global).unwrap();
        assert_eq!(global.name, "scoped");
        assert!(global_plugins.join("scoped/plugin.json").exists());
        assert!(!project_plugins.join("scoped").exists());
        assert_eq!(
            mgr.scope_of_path(&global.source_path),
            PluginInstallScope::Global
        );
        mgr.remove("scoped").unwrap();

        write_plugin(
            &src.path,
            "scoped",
            "1.0.0",
            r#"{"post_write": {"script": "hooks/hook.sh"}}"#,
        );
        let local = mgr
            .install(&src.path, PluginInstallScope::Workspace)
            .unwrap();
        assert!(project_plugins.join("scoped/plugin.json").exists());
        assert!(project_plugins.join("scoped/plugin.json").exists());
        assert_eq!(
            mgr.scope_of_path(&local.source_path),
            PluginInstallScope::Workspace
        );
        // Reloads without trust because of the user-install marker.
        let mgr2 = PluginManager::new_with_user_plugins_dir(
            project_plugins,
            Some(global_plugins),
            ws,
            false,
        );
        assert!(mgr2.list().contains_key("scoped"));
        assert!(mgr2.skipped_project_plugins().is_empty());
    }

    #[test]
    fn relative_plugins_dir_resolves_against_workspace() {
        let tmp = TmpDir::new("mgr_rel_dir");
        let ws = tmp.path.join("workspace");
        fs::create_dir_all(ws.join(".catalyst-code/plugins")).unwrap();

        // Relative plugins_dir must not depend on process cwd.
        let mgr = PluginManager::new(PathBuf::from(".catalyst-code/plugins"), ws.clone(), false);

        let src = TmpDir::new("rel_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "relplug",
            "1.0.0",
            r#"{"post_write": {"script": "hooks/hook.sh"}}"#,
        );

        mgr.install(&src.path, PluginInstallScope::Workspace)
            .unwrap();
        assert!(ws
            .join(".catalyst-code/plugins/relplug/plugin.json")
            .exists());
        assert!(ws
            .join(".catalyst-code/plugins/relplug/plugin.json")
            .exists());

        // Simulate restart: new manager, trust still off.
        let mgr2 = PluginManager::new(PathBuf::from(".catalyst-code/plugins"), ws, false);
        assert!(
            mgr2.list().contains_key("relplug"),
            "workspace user-install must survive restart without trust"
        );
    }

    #[test]
    fn plugin_install_scope_parse() {
        assert_eq!(
            PluginInstallScope::parse("global").unwrap(),
            PluginInstallScope::Global
        );
        assert_eq!(
            PluginInstallScope::parse("--workspace").unwrap(),
            PluginInstallScope::Workspace
        );
        assert!(PluginInstallScope::parse("nope").is_err());
    }

    #[test]
    fn install_skips_venv_and_git() {
        let tmp = TmpDir::new("mgr_skip_venv");
        let mgr = PluginManager::new(
            tmp.path.join("managed"),
            PathBuf::from("/__pm_test_ws__"),
            true,
        );

        let src = TmpDir::new("skip_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "hook.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "clean",
            "1.0.0",
            r#"{"post_write": {"script": "hooks/hook.sh"}}"#,
        );
        // Junk that must NOT be copied (lib64 symlink is what broke local installs).
        fs::create_dir_all(src.path.join(".venv/lib")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("lib", src.path.join(".venv/lib64")).unwrap();
        fs::write(src.path.join(".venv/pyvenv.cfg"), "home = /usr\n").unwrap();
        fs::create_dir_all(src.path.join(".git")).unwrap();
        fs::write(src.path.join(".git/config"), "x").unwrap();

        let installed = mgr
            .install(&src.path, PluginInstallScope::Workspace)
            .unwrap();
        assert_eq!(installed.name, "clean");
        let dest = tmp.path.join("managed/clean");
        assert!(dest.join("plugin.json").exists());
        assert!(
            !dest.join(".venv").exists(),
            ".venv must be skipped on install"
        );
        assert!(
            !dest.join(".git").exists(),
            ".git must be skipped on install"
        );
    }

    #[test]
    fn install_rejects_duplicate() {
        let tmp = TmpDir::new("mgr_dup");
        let mgr = PluginManager::new(
            tmp.path.join("managed"),
            PathBuf::from("/__pm_test_ws__"),
            true,
        );

        let src = TmpDir::new("dup_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "h.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "dup",
            "1.0.0",
            r#"{"pre_read": {"script": "hooks/h.sh"}}"#,
        );

        mgr.install(&src.path, PluginInstallScope::Workspace)
            .unwrap();
        let result = mgr.install(&src.path, PluginInstallScope::Workspace);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("already installed"));
    }

    #[test]
    fn parse_github_plugin_source_forms() {
        let cases = [
            (
                "https://github.com/acme/cool-plugin",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: None,
                    subdir: None,
                },
            ),
            (
                "https://github.com/acme/cool-plugin.git",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: None,
                    subdir: None,
                },
            ),
            (
                "https://github.com/acme/cool-plugin@v1.2.0",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: Some("v1.2.0".into()),
                    subdir: None,
                },
            ),
            (
                "acme/cool-plugin@v1.2.0:plugins/foo",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: Some("v1.2.0".into()),
                    subdir: Some("plugins/foo".into()),
                },
            ),
            (
                "acme/cool-plugin:plugins/foo",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: None,
                    subdir: Some("plugins/foo".into()),
                },
            ),
            (
                "https://github.com/acme/cool-plugin/releases/tag/v2.0.0",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: Some("v2.0.0".into()),
                    subdir: None,
                },
            ),
            (
                "https://github.com/acme/cool-plugin/tree/v2.0.0/plugins/foo",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: Some("v2.0.0".into()),
                    subdir: Some("plugins/foo".into()),
                },
            ),
            (
                "git@github.com:acme/cool-plugin.git",
                GithubPluginSource {
                    owner: "acme".into(),
                    repo: "cool-plugin".into(),
                    tag: None,
                    subdir: None,
                },
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_github_plugin_source(input).as_ref(),
                Some(&expected),
                "input={input}"
            );
        }
        assert!(parse_github_plugin_source("/abs/path/to/plugin").is_none());
        assert!(parse_github_plugin_source("./relative/plugin").is_none());
        assert!(parse_github_plugin_source("https://gitlab.com/acme/cool").is_none());
    }

    #[test]
    fn resolve_plugin_dir_root_and_subdir() {
        let tmp = TmpDir::new("resolve_plugin_dir");
        write_plugin(&tmp.path, "root-plug", "1.0.0", "{}");
        assert_eq!(
            resolve_plugin_dir(&tmp.path, None).unwrap(),
            tmp.path.clone()
        );

        let mono = TmpDir::new("resolve_mono");
        let nested = mono.path.join("plugins/foo");
        fs::create_dir_all(&nested).unwrap();
        write_plugin(&nested, "foo", "1.0.0", "{}");
        assert_eq!(
            resolve_plugin_dir(&mono.path, Some("plugins/foo")).unwrap(),
            nested
        );
        let err = resolve_plugin_dir(&mono.path, None).unwrap_err();
        assert!(err.contains("no plugin.json") || err.contains("pass owner/repo"));
    }

    #[test]
    fn extract_zip_and_find_top_level() {
        let tmp = TmpDir::new("zip_extract");
        // Build a minimal zip shaped like a GitHub zipball.
        let zip_path = tmp.path.join("src.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::FileOptions::<()>::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.add_directory("acme-cool-abc123/", opts).unwrap();
            zip.start_file("acme-cool-abc123/plugin.json", opts)
                .unwrap();
            let manifest = r#"{"name":"cool","version":"1.0.0"}"#;
            use std::io::Write;
            zip.write_all(manifest.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let bytes = fs::read(&zip_path).unwrap();
        let extract = tmp.path.join("out");
        fs::create_dir_all(&extract).unwrap();
        extract_zip_bytes(&bytes, &extract).unwrap();
        let root = find_single_top_level_dir(&extract).unwrap();
        assert!(root.join("plugin.json").is_file());
        let plugin_dir = resolve_plugin_dir(&root, None).unwrap();
        assert_eq!(plugin_dir, root);
    }

    #[test]
    fn write_plugin_source_meta_roundtrip() {
        let tmp = TmpDir::new("source_meta");
        write_plugin(&tmp.path, "meta-plug", "1.0.0", "{}");
        let meta = PluginSourceMeta {
            kind: "github_release".into(),
            owner: "acme".into(),
            repo: "cool".into(),
            tag: "v1.2.0".into(),
            url: "https://github.com/acme/cool".into(),
            subdir: None,
            installed_at: 1_700_000_000,
        };
        write_plugin_source_meta(&tmp.path, &meta).unwrap();
        let raw = fs::read_to_string(tmp.path.join(PLUGIN_SOURCE_META_FILE)).unwrap();
        let parsed: PluginSourceMeta = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, meta);
    }

    #[test]
    fn enable_disable_toggle() {
        let tmp = TmpDir::new("mgr_toggle");
        let mgr = PluginManager::new(
            tmp.path.join("managed"),
            PathBuf::from("/__pm_test_ws__"),
            true,
        );

        let src = TmpDir::new("toggle_src");
        fs::create_dir_all(src.path.join("hooks")).unwrap();
        write_hook_script(&src.path.join("hooks"), "h.sh", r#"{"allow":true}"#, 0);
        write_plugin(
            &src.path,
            "toggle-me",
            "1.0.0",
            r#"{"pre_write": {"script": "hooks/h.sh"}}"#,
        );

        mgr.install(&src.path, PluginInstallScope::Workspace)
            .unwrap();

        // Initially enabled.
        assert!(mgr.get_plugin("toggle-me").unwrap().enabled);

        mgr.disable("toggle-me").unwrap();
        assert!(!mgr.get_plugin("toggle-me").unwrap().enabled);

        mgr.enable("toggle-me").unwrap();
        assert!(mgr.get_plugin("toggle-me").unwrap().enabled);

        // Disabled plugins are excluded from hook configs.
        mgr.disable("toggle-me").unwrap();
        let configs = mgr.get_hook_configs("pre_write");
        assert!(configs.is_empty());
    }

    #[test]
    fn enable_disable_unknown_is_error() {
        let tmp = TmpDir::new("mgr_unknown");
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__pm_test_ws__"), true);
        assert!(mgr.enable("nope").is_err());
        assert!(mgr.disable("nope").is_err());
        assert!(mgr.remove("nope").is_err());
    }

    // ---- execute_hook ----

    #[tokio::test]
    async fn execute_hook_allow() {
        let tmp = TmpDir::new("exec_allow");
        let script = write_hook_script(
            &tmp.path,
            "allow.sh",
            r#"{"allow": true, "reason": "all good"}"#,
            0,
        );
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(result.allow);
        assert_eq!(result.reason, "all good");
        assert!(result.modify.is_none());
    }

    #[tokio::test]
    async fn execute_hook_deny() {
        let tmp = TmpDir::new("exec_deny");
        let script = write_hook_script(
            &tmp.path,
            "deny.sh",
            r#"{"allow": false, "reason": "blocked"}"#,
            0,
        );
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(!result.allow);
        assert_eq!(result.reason, "blocked");
    }

    #[test]
    fn apply_modify_overrides_only_listed_keys() {
        // pre_write: reformat content, keep path/edits.
        let mut args = json!({ "path": "src/lib.rs", "content": "fn main(){}" });
        let modify = json!({ "content": "fn main() {\n}\n" });
        apply_modify(&mut args, &modify);
        assert_eq!(args["path"], json!("src/lib.rs"));
        assert_eq!(args["content"], json!("fn main() {\n}\n"));
    }

    #[test]
    fn apply_modify_pre_bash_replaces_command_only() {
        let mut args = json!({ "command": "rm -rf /", "timeout": 30 });
        let modify = json!({ "command": "echo safe" });
        apply_modify(&mut args, &modify);
        assert_eq!(args["command"], json!("echo safe"));
        assert_eq!(
            args["timeout"],
            json!(30),
            "unrelated keys must be preserved"
        );
    }

    #[test]
    fn apply_modify_pre_read_redirects_path() {
        let mut args = json!({ "path": "a.txt", "context": 3 });
        let modify = json!({ "path": "b.txt" });
        apply_modify(&mut args, &modify);
        assert_eq!(args["path"], json!("b.txt"));
        assert_eq!(args["context"], json!(3));
    }

    #[test]
    fn apply_modify_composes_across_hooks() {
        // Two hooks amend different fields; both survive.
        let mut args = json!({ "path": "f", "content": "x" });
        apply_modify(&mut args, &json!({ "content": "y" }));
        apply_modify(&mut args, &json!({ "path": "g" }));
        assert_eq!(args, json!({ "path": "g", "content": "y" }));
    }

    #[test]
    fn apply_modify_non_object_modify_is_noop() {
        let mut args = json!({ "path": "f", "content": "x" });
        apply_modify(&mut args, &json!("not an object"));
        apply_modify(&mut args, &json!(42));
        apply_modify(&mut args, &json!(null));
        assert_eq!(args, json!({ "path": "f", "content": "x" }));
    }

    #[test]
    fn apply_modify_non_object_args_is_noop() {
        let mut args = json!("scalar args");
        apply_modify(&mut args, &json!({ "content": "y" }));
        assert_eq!(args, json!("scalar args"));
    }

    #[tokio::test]
    async fn execute_hook_with_modify() {
        let tmp = TmpDir::new("exec_modify");
        let response = r#"{"allow": true, "reason": "reformatted", "modify": {"content": "new"}}"#;
        let script = write_hook_script(&tmp.path, "modify.sh", response, 0);
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(result.allow);
        assert_eq!(result.reason, "reformatted");
        assert_eq!(result.modify, Some(json!({"content": "new"})));
    }

    #[tokio::test]
    async fn execute_hook_nonzero_exit_pre_denies() {
        let tmp = TmpDir::new("exec_exit_pre");
        let script = write_hook_script(
            &tmp.path,
            "fail.sh",
            r#"{"allow": true}"#,
            1, // exits with 1
        );
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(!result.allow);
        assert!(result.reason.contains("exited"));
    }

    #[tokio::test]
    async fn execute_hook_nonzero_exit_post_skips() {
        let tmp = TmpDir::new("exec_exit_post");
        let script = write_hook_script(&tmp.path, "fail.sh", r#"{"allow": true}"#, 1);
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("post_bash", "bash", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("post_bash", "test-plugin", &config, &ctx).await;
        // post hooks: non-zero exit is skipped, operation continues.
        assert!(result.allow);
        assert!(result.reason.contains("exited"));
        assert!(result.modify.is_none());
    }

    #[tokio::test]
    async fn execute_hook_bad_json_pre_denies() {
        let tmp = TmpDir::new("exec_bad_json");
        let script = write_hook_script(&tmp.path, "bad.sh", "NOT JSON", 0);
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(!result.allow);
        assert!(result.reason.contains("invalid JSON"));
    }

    #[tokio::test]
    async fn execute_hook_timeout_pre_denies() {
        let tmp = TmpDir::new("exec_timeout");
        // Script sleeps long enough to trigger the timeout.
        let script = tmp.path.join("sleep.sh");
        fs::write(&script, "#!/bin/sh\nsleep 10\necho '{\"allow\":true}'\n").unwrap();
        make_executable(&script);

        let config = HookConfig {
            script,
            timeout_ms: 200, // very short timeout
            pass_args: false,
        };
        let ctx = build_context("pre_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_write", "test-plugin", &config, &ctx).await;
        assert!(!result.allow);
        assert!(result.reason.contains("timed out"));
    }

    #[tokio::test]
    async fn execute_hook_post_always_allows_even_on_deny() {
        // For post hooks, even if the hook returns allow:false, we don't block.
        let tmp = TmpDir::new("exec_post_deny");
        let script = write_hook_script(
            &tmp.path,
            "deny.sh",
            r#"{"allow": false, "reason": "saw an issue"}"#,
            0,
        );
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("post_write", "write_file", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("post_write", "test-plugin", &config, &ctx).await;
        assert!(result.allow);
        assert_eq!(result.modify, None);
    }

    #[tokio::test]
    async fn execute_hook_empty_stdout_pre_denies() {
        let tmp = TmpDir::new("exec_empty");
        let script = write_hook_script(&tmp.path, "empty.sh", "", 0);
        let config = HookConfig {
            script,
            timeout_ms: 5000,
            pass_args: false,
        };
        let ctx = build_context("pre_bash", "bash", "/ws", None, "sess.jsonl", false);
        let result = execute_hook("pre_bash", "test-plugin", &config, &ctx).await;
        assert!(!result.allow);
        assert!(result.reason.contains("empty stdout"));
    }

    // ---- build_context ----

    #[test]
    fn build_context_structure() {
        let ctx = build_context(
            "pre_write",
            "write_file",
            "/home/user/project",
            Some(&json!({"path": "src/main.rs", "content": "fn main() {}"})),
            "session_123.jsonl",
            true,
        );

        assert_eq!(ctx["hook"], "pre_write");
        assert_eq!(ctx["tool"], "write_file");
        assert_eq!(ctx["workspace"], "/home/user/project");
        assert_eq!(ctx["session_id"], "session_123.jsonl");
        assert!(ctx["timestamp"].as_u64().is_some());
        assert_eq!(ctx["args"]["path"], "src/main.rs");
        assert_eq!(ctx["args"]["content"], "fn main() {}");
    }

    #[test]
    fn build_context_omits_args_when_pass_args_false() {
        let ctx = build_context(
            "pre_write",
            "write_file",
            "/ws",
            Some(&json!({"secret": "value"})),
            "sess.jsonl",
            false,
        );
        assert!(ctx.get("args").is_none());
    }

    #[test]
    fn build_context_handles_none_args() {
        let ctx = build_context("session_start", "", "/ws", None, "sess.jsonl", true);
        assert!(ctx.get("args").is_none());
    }

    // ---- default timeouts ----

    #[test]
    fn pre_hooks_get_short_timeout() {
        assert_eq!(default_hook_timeout("pre_bash"), 5_000);
        assert_eq!(default_hook_timeout("pre_write"), 5_000);
        assert_eq!(default_hook_timeout("pre_read"), 5_000);
        assert_eq!(default_hook_timeout("pre_tool"), 5_000);
        assert_eq!(default_hook_timeout("pre_input"), 5_000);
        // pre_compact / pre_turn / pre_agent_start / pre_context are advisory
        // lifecycle hooks (policy table), not blocking tool gates — 30s.
        assert_eq!(default_hook_timeout("pre_compact"), 30_000);
        assert_eq!(default_hook_timeout("pre_turn"), 30_000);
        assert_eq!(default_hook_timeout("pre_agent_start"), 30_000);
        assert_eq!(default_hook_timeout("pre_context"), 30_000);
    }

    #[test]
    fn post_hooks_get_long_timeout() {
        assert_eq!(default_hook_timeout("post_bash"), 30_000);
        assert_eq!(default_hook_timeout("post_write"), 30_000);
        assert_eq!(default_hook_timeout("session_start"), 30_000);
        assert_eq!(default_hook_timeout("session_stop"), 30_000);
        assert_eq!(default_hook_timeout("session_shutdown"), 30_000);
        assert_eq!(default_hook_timeout("turn_start"), 30_000);
        assert_eq!(default_hook_timeout("turn_end"), 30_000);
    }

    #[test]
    fn hook_policy_deny_vs_skip() {
        assert_eq!(hook_policy("pre_bash").fail_mode, HookFailMode::Deny);
        assert_eq!(hook_policy("pre_tool").fail_mode, HookFailMode::Deny);
        assert_eq!(hook_policy("pre_input").fail_mode, HookFailMode::Deny);
        assert!(hook_policy("pre_bash").honor_allow);
        assert_eq!(hook_policy("post_tool").fail_mode, HookFailMode::Skip);
        assert_eq!(hook_policy("pre_context").fail_mode, HookFailMode::Skip);
        assert!(!hook_policy("pre_context").honor_allow);
        assert!(hook_policy("pre_context").honor_modify);
        assert_eq!(
            hook_policy("session_shutdown").fail_mode,
            HookFailMode::Skip
        );
    }

    // ---- plugin-declared tools ----

    /// Write a plugin that declares one tool whose handler is a minimal
    /// executable script, into a `tools-plugin/` SUBDIRECTORY of `dir` (so a
    /// `PluginManager::new(dir, …)` scan finds it — the scanner loads each
    /// subdirectory, not `dir` itself). Returns the plugin directory path so
    /// `load_plugin_from_dir` callers can target it directly. `extra` is
    /// spliced into the tool object as extra fields (e.g.
    /// `"kind":"readonly","timeout_ms":12345`).
    fn write_plugin_with_tool(dir: &Path, tool_name: &str, extra: &str) -> PathBuf {
        let plugin_dir = dir.join("tools-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        let tools_dir = plugin_dir.join("tools");
        fs::create_dir_all(&tools_dir).unwrap();
        let script = tools_dir.join("run.sh");
        fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&script);
        let tool_extra = if extra.is_empty() {
            String::new()
        } else {
            format!(", {}", extra)
        };
        let manifest = format!(
            r#"{{
  "name": "tools-plugin",
  "version": "1.0.0",
  "tools": [
    {{ "name": "{name}", "description": "a tool", "parameters": {{"type":"object","properties":{{}}}}, "script": "tools/run.sh"{extra} }}
  ]
}}"#,
            name = tool_name,
            extra = tool_extra,
        );
        fs::write(plugin_dir.join("plugin.json"), &manifest).unwrap();
        plugin_dir
    }

    #[test]
    fn load_plugin_with_tools() {
        let tmp = TmpDir::new("load_tools");
        let pdir = write_plugin_with_tool(&tmp.path, "my_tool", "");
        let plugin = PluginManager::load_plugin_from_dir(&pdir).unwrap();
        assert_eq!(plugin.tools.len(), 1);
        let t = &plugin.tools[0];
        assert_eq!(t.name, "my_tool");
        assert_eq!(t.description, "a tool");
        assert_eq!(t.timeout_ms, DEFAULT_POST_TIMEOUT_MS);
        assert_eq!(t.kind, ToolKind::Destructive); // default
        assert!(t.parameters.is_object());
        assert!(t.script.exists());
    }

    #[test]
    fn load_tool_readonly_kind() {
        let tmp = TmpDir::new("tool_readonly");
        let pdir = write_plugin_with_tool(&tmp.path, "ro_tool", r#""kind":"readonly""#);
        let plugin = PluginManager::load_plugin_from_dir(&pdir).unwrap();
        assert_eq!(plugin.tools[0].kind, ToolKind::ReadOnly);
    }

    #[test]
    fn load_tool_explicit_destructive_kind() {
        let tmp = TmpDir::new("tool_destructive");
        let pdir = write_plugin_with_tool(&tmp.path, "d_tool", r#""kind":"destructive""#);
        let plugin = PluginManager::load_plugin_from_dir(&pdir).unwrap();
        assert_eq!(plugin.tools[0].kind, ToolKind::Destructive);
    }

    #[test]
    fn load_tool_custom_timeout() {
        let tmp = TmpDir::new("tool_timeout");
        let pdir = write_plugin_with_tool(&tmp.path, "t_tool", r#""timeout_ms": 12345"#);
        let plugin = PluginManager::load_plugin_from_dir(&pdir).unwrap();
        assert_eq!(plugin.tools[0].timeout_ms, 12345);
    }

    #[test]
    fn load_tool_rejects_missing_script() {
        let tmp = TmpDir::new("tool_missing");
        fs::write(
            tmp.path.join("plugin.json"),
            r#"{"name":"p","version":"1.0.0","tools":[{"name":"x","script":"tools/nope.sh"}]}"#,
        )
        .unwrap();
        let r = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn load_tool_rejects_path_escape() {
        let tmp = TmpDir::new("tool_escape");
        fs::write(
            tmp.path.join("plugin.json"),
            r#"{"name":"p","version":"1.0.0","tools":[{"name":"x","script":"../escape.sh"}]}"#,
        )
        .unwrap();
        let r = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("escapes"));
    }

    #[test]
    fn load_tool_rejects_invalid_kind() {
        let tmp = TmpDir::new("tool_bad_kind");
        let pdir = write_plugin_with_tool(&tmp.path, "bad", r#""kind":"weird""#);
        let r = PluginManager::load_plugin_from_dir(&pdir);
        assert!(r.is_err());
        let e = r.unwrap_err();
        assert!(e.contains("invalid kind"));
        assert!(e.contains("weird"));
    }

    #[test]
    fn load_tool_rejects_empty_name() {
        let tmp = TmpDir::new("tool_empty_name");
        fs::write(
            tmp.path.join("plugin.json"),
            r#"{"name":"p","version":"1.0.0","tools":[{"name":"","script":"tools/run.sh"}]}"#,
        )
        .unwrap();
        let r = PluginManager::load_plugin_from_dir(&tmp.path);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("empty name"));
    }

    #[test]
    fn tool_definitions_schema() {
        let tmp = TmpDir::new("tool_defs");
        write_plugin_with_tool(&tmp.path, "echo_tool", r#""timeout_ms":5000"#);
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        let defs = mgr.tool_definitions();
        assert_eq!(defs.len(), 1);
        let f = defs[0].get("function").unwrap();
        assert_eq!(f.get("name").and_then(|v| v.as_str()), Some("echo_tool"));
        assert_eq!(
            f.get("description").and_then(|v| v.as_str()),
            Some("a tool")
        );
        assert!(f.get("parameters").unwrap().is_object());
    }

    #[test]
    fn tool_definitions_excludes_disabled() {
        let tmp = TmpDir::new("tool_disabled");
        write_plugin_with_tool(&tmp.path, "t", "");
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert_eq!(mgr.tool_definitions().len(), 1);
        mgr.disable("tools-plugin").unwrap();
        assert_eq!(mgr.tool_definitions().len(), 0);
        assert!(mgr.tool_config("t").is_none());
        assert!(mgr.tool_kind("t").is_none());
    }

    #[test]
    fn tool_config_and_kind_lookup() {
        let tmp = TmpDir::new("tool_lookup");
        write_plugin_with_tool(&tmp.path, "lookup", r#""kind":"readonly""#);
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        let tc = mgr.tool_config("lookup").unwrap();
        assert_eq!(tc.name, "lookup");
        assert_eq!(tc.kind, ToolKind::ReadOnly);
        assert!(mgr.tool_config("nope").is_none());
        assert_eq!(mgr.tool_kind("lookup"), Some(ToolKind::ReadOnly));
        // A built-in tool name is not a plugin tool → None.
        assert!(mgr.tool_kind("read_file").is_none());
    }

    #[test]
    fn builtin_name_collision_never_hijacks() {
        // A plugin MAY declare a tool whose name collides with a built-in. It
        // still loads (tool_config finds it), but the registry merge hides it
        // from the model's tool list (built-in wins) and the dispatch +
        // classify guards drop it via `is_builtin` — so the built-in always
        // runs, never the same-named plugin tool. This test pins that guard
        // composition so a future change can't reintroduce the hijack.
        let tmp = TmpDir::new("tool_collision");
        write_plugin_with_tool(&tmp.path, "read_file", r#""kind":"readonly""#);
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        // Loadable + findable by the manager …
        assert!(mgr.tool_config("read_file").is_some());
        assert_eq!(mgr.tool_kind("read_file"), Some(ToolKind::ReadOnly));
        // … but the dispatch guard filters it out (it's a built-in name), so a
        // call to "read_file" routes to the built-in, not the plugin.
        assert!(mgr
            .tool_config("read_file")
            .filter(|_| !crate::tools::is_builtin("read_file"))
            .is_none());
        // And a genuinely-custom name is NOT filtered — it dispatches normally.
        let tmp2 = TmpDir::new("tool_no_collision");
        write_plugin_with_tool(&tmp2.path, "my_domain_tool", r#""kind":"readonly""#);
        let mgr2 = PluginManager::new(tmp2.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr2
            .tool_config("my_domain_tool")
            .filter(|_| !crate::tools::is_builtin("my_domain_tool"))
            .is_some());
        // is_builtin itself: known built-ins true, arbitrary/empty false.
        assert!(crate::tools::is_builtin("read_file"));
        assert!(crate::tools::is_builtin("bash"));
        assert!(!crate::tools::is_builtin("my_domain_tool"));
        assert!(!crate::tools::is_builtin(""));
    }

    // ---- execute_plugin_tool ----

    /// Write a `plugin.json` with arbitrary manifest JSON into `dir`.
    fn write_manifest(dir: &Path, manifest: &str) {
        fs::write(dir.join("plugin.json"), manifest).unwrap();
    }

    #[test]
    fn hook_points_include_catch_all() {
        assert!(HOOK_POINTS.contains(&"pre_tool"));
        assert!(HOOK_POINTS.contains(&"post_tool"));
        assert!(HOOK_POINTS.contains(&"pre_input"));
        assert!(HOOK_POINTS.contains(&"pre_agent_start"));
        assert!(HOOK_POINTS.contains(&"pre_context"));
        assert!(HOOK_POINTS.contains(&"turn_start"));
        assert!(HOOK_POINTS.contains(&"turn_end"));
        assert!(HOOK_POINTS.contains(&"session_shutdown"));
        // Every HOOK_POINTS entry must have an explicit policy (no silent
        // fallback for known points).
        for h in HOOK_POINTS {
            let _ = hook_policy(h);
        }
        // pre_tool gets the short timeout; post_tool the long one.
        assert_eq!(default_hook_timeout("pre_tool"), DEFAULT_PRE_TIMEOUT_MS);
        assert_eq!(default_hook_timeout("post_tool"), DEFAULT_POST_TIMEOUT_MS);
    }

    #[test]
    fn disable_tools_manifest_loaded() {
        let tmp = TmpDir::new("disable_loaded");
        write_manifest(
            &tmp.path,
            r#"{"name":"no-bash","version":"1.0.0","disable_tools":["bash","git_commit"]}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(
            plugin.disable_tools,
            vec!["bash".to_string(), "git_commit".to_string()]
        );
    }

    #[test]
    fn system_prompt_manifest_loaded() {
        let tmp = TmpDir::new("sysprompt_loaded");
        write_manifest(
            &tmp.path,
            r#"{"name":"rules","version":"2.0.0","system_prompt":"Never run raw SQL."}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(plugin.system_prompt, "Never run raw SQL.");
    }

    #[test]
    fn memory_provider_manifest_loaded() {
        let tmp = TmpDir::new("memprov_loaded");
        let mdir = tmp.path.join("memory");
        fs::create_dir_all(&mdir).unwrap();
        write_hook_script(
            &mdir,
            "provider.sh",
            r#"{"ok":true,"injection":"[PERSISTENT MEMORIES]\n- test"}"#,
            0,
        );
        write_manifest(
            &tmp.path,
            r#"{"name":"engraphis-memory","version":"1.0.0","memory_provider":{
               "script":"memory/provider.sh","timeout_ms":12000
            }}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let mp = plugin.memory_provider.expect("memory_provider loaded");
        assert_eq!(mp.plugin_name, "engraphis-memory");
        assert_eq!(mp.timeout_ms, 12_000);
        assert!(mp.script.ends_with("provider.sh"));
    }

    #[test]
    fn memory_provider_rejects_missing_script() {
        let tmp = TmpDir::new("memprov_missing");
        write_manifest(
            &tmp.path,
            r#"{"name":"bad-mem","version":"1.0.0","memory_provider":{"script":"memory/nope.py"}}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(
            err.contains("does not exist") || err.contains("not executable"),
            "err={err}"
        );
    }

    #[tokio::test]
    async fn execute_memory_provider_inject() {
        let tmp = TmpDir::new("memprov_exec");
        let mdir = tmp.path.join("memory");
        fs::create_dir_all(&mdir).unwrap();
        // Read stdin (discard) then emit a fixed inject response.
        let script = mdir.join("provider.sh");
        fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\necho '{\"ok\":true,\"injection\":\"[PERSISTENT MEMORIES]\\n- hello\"}'\n",
        )
        .unwrap();
        make_executable(&script);
        write_manifest(
            &tmp.path,
            r#"{"name":"mem","version":"1.0.0","memory_provider":{"script":"memory/provider.sh"}}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let mp = plugin.memory_provider.unwrap();
        let result = execute_memory_provider(&mp, "inject", &json!({}), "/tmp/ws", "sess").await;
        assert!(result.ok, "output={}", result.output);
        assert!(result.injection.contains("PERSISTENT MEMORIES"));
        assert!(result.injection.contains("hello"));
    }

    #[test]
    fn override_field_loaded() {
        let tmp = TmpDir::new("override_loaded");
        let pdir = write_plugin_with_tool(&tmp.path, "bash", r#""override":true"#);
        let plugin = PluginManager::load_plugin_from_dir(&pdir).unwrap();
        assert!(plugin.tools[0].override_builtin);

        // Without override, it stays false.
        let tmp2 = TmpDir::new("override_false");
        let pdir2 = write_plugin_with_tool(&tmp2.path, "my_tool", "");
        let plugin2 = PluginManager::load_plugin_from_dir(&pdir2).unwrap();
        assert!(!plugin2.tools[0].override_builtin);
    }

    // ---- plugin-declared OAuth provider loading ----

    #[test]
    fn load_oauth_minimal() {
        let tmp = TmpDir::new("oauth_minimal");
        let odir = tmp.path.join("oauth");
        fs::create_dir_all(&odir).unwrap();
        write_hook_script(&odir, "oauth.sh", r#"{"access_token":null}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{"name":"grok-oauth","version":"0.1.0","oauth":{
               "provider_id":"grok",
               "base_url":"https://api.x.ai/v1",
               "script":"oauth/oauth.sh"
            }}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let oauth = plugin.oauth.expect("oauth config loaded");
        assert_eq!(oauth.provider_id, "grok");
        assert_eq!(oauth.base_url, "https://api.x.ai/v1");
        assert_eq!(oauth.kind, ProviderKind::OpenAI);
        assert_eq!(oauth.label, "grok"); // defaults to provider_id
                                         // token_path defaults to <provider_id>.json under the oauth dir.
        assert!(oauth.token_path.ends_with("grok.json"));
        // The shared script resolves for every action.
        assert!(oauth.script_for("login").is_some());
        assert!(oauth.script_for("complete").is_some());
        assert!(oauth.script_for("token").is_some());
        assert!(oauth.script_for("clear").is_some());
    }

    #[test]
    fn oauth_login_supports_automatic_and_existing_flows() {
        let poll = json!({"flow":"poll","auto_complete":true});
        assert!(oauth_flow_is_auto(&poll));
        assert!(!oauth_flow_is_existing(&poll));

        let existing = json!({
            "ok": true,
            "flow": "already_authenticated"
        });
        assert!(oauth_flow_is_existing(&existing));
        assert!(!oauth_flow_is_auto(&existing));

        let failed = json!({
            "ok": false,
            "flow": "already_authenticated",
            "error": "not signed in"
        });
        assert!(!oauth_flow_is_existing(&failed));
        assert_eq!(oauth_login_error(&failed).as_deref(), Some("not signed in"));
    }

    #[test]
    fn oauth_detect_path_resolves_codex_home_form() {
        let tmp = TmpDir::new("oauth_detect_path");
        let odir = tmp.path.join("oauth");
        fs::create_dir_all(&odir).unwrap();
        write_hook_script(&odir, "oauth.sh", r#"{"access_token":null}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{"name":"codex","version":"0.1.0","oauth":{
               "provider_id":"codex","base_url":"https://chatgpt.com/backend-api/codex",
               "detect_path":".codex/auth.json","script":"oauth/oauth.sh"
            }}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let oauth = plugin.oauth.unwrap();
        let home = crate::config::home_dir().expect("test HOME");
        assert_eq!(oauth.detect_path, Some(home.join(".codex/auth.json")));
    }

    #[test]
    fn load_oauth_rejects_missing_token_script() {
        let tmp = TmpDir::new("oauth_no_token");
        write_manifest(
            &tmp.path,
            r#"{"name":"bad","version":"0.1.0","oauth":{
               "provider_id":"bad","base_url":"https://x.example/v1"
            }}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("no token script"), "got: {err}");
    }

    #[test]
    fn load_oauth_rejects_invalid_kind() {
        let tmp = TmpDir::new("oauth_bad_kind");
        let odir = tmp.path.join("oauth");
        fs::create_dir_all(&odir).unwrap();
        write_hook_script(&odir, "oauth.sh", r#"{"access_token":null}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{"name":"bad","version":"0.1.0","oauth":{
               "provider_id":"bad","base_url":"https://x.example/v1",
               "kind":"weird","script":"oauth/oauth.sh"
            }}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("invalid kind"), "got: {err}");
    }

    #[test]
    fn load_oauth_per_action_overrides() {
        let tmp = TmpDir::new("oauth_overrides");
        let odir = tmp.path.join("oauth");
        fs::create_dir_all(&odir).unwrap();
        write_hook_script(&odir, "login.sh", r#"{"url":"https://x"}"#, 0);
        write_hook_script(&odir, "complete.sh", r#"{"ok":true}"#, 0);
        write_hook_script(&odir, "token.sh", r#"{"access_token":"t"}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{"name":"ov","version":"0.1.0","oauth":{
               "provider_id":"ov","base_url":"https://x.example/v1","kind":"anthropic",
               "login_script":"oauth/login.sh","complete_script":"oauth/complete.sh",
               "token_script":"oauth/token.sh"
            }}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let oauth = plugin.oauth.unwrap();
        assert_eq!(oauth.kind, ProviderKind::Anthropic);
        // No shared script → only the per-action overrides resolve.
        assert!(oauth.script_for("login").is_some());
        assert!(oauth.script_for("complete").is_some());
        assert!(oauth.script_for("token").is_some());
        assert!(oauth.script_for("clear").is_none());
    }

    #[test]
    fn oauth_provider_config_builds_config() {
        let tmp = TmpDir::new("oauth_provider_cfg");
        // PluginManager::new scans SUBDIRS of plugins_dir, so the plugin lives
        // in its own subdir.
        let pdir = tmp.path.join("grok-oauth");
        let odir = pdir.join("oauth");
        fs::create_dir_all(&odir).unwrap();
        write_hook_script(&odir, "oauth.sh", r#"{"access_token":null}"#, 0);
        write_manifest(
            &pdir,
            r#"{"name":"grok-oauth","version":"0.1.0","oauth":{
               "provider_id":"grok","base_url":"https://api.x.ai/v1",
               "headers":[["X-Source","cc"]],"script":"oauth/oauth.sh"
            }}"#,
        );
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr.supports_oauth_login("grok"));
        let pc = mgr.oauth_provider_config("grok").expect("provider config");
        assert_eq!(pc.name, "grok");
        assert_eq!(pc.base_url, "https://api.x.ai/v1");
        assert_eq!(pc.kind, ProviderKind::OpenAI);
        assert!(pc.api_key.is_none());
        assert_eq!(pc.headers, vec![("X-Source".to_string(), "cc".to_string())]);
        // Unknown provider_id → None / not supported.
        assert!(mgr.oauth_provider_config("nope").is_none());
        assert!(!mgr.supports_oauth_login("nope"));
    }

    #[test]
    fn manager_disabled_tools_unions_across_plugins() {
        let tmp = TmpDir::new("disable_union");
        let a = tmp.path.join("a");
        let b = tmp.path.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        write_manifest(
            &a,
            r#"{"name":"a","version":"1.0.0","disable_tools":["bash"]}"#,
        );
        write_manifest(
            &b,
            r#"{"name":"b","version":"1.0.0","disable_tools":["bash","edit"]}"#,
        );
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        let disabled = mgr.disabled_tools();
        assert!(disabled.contains("bash"));
        assert!(disabled.contains("edit"));
        assert_eq!(disabled.len(), 2);
    }

    #[test]
    fn overridden_tool_names_only_when_override_and_builtin() {
        // override:true on a built-in name → overridden. override:false (or a
        // custom name) → NOT overridden.
        let tmp = TmpDir::new("override_names");
        write_plugin_with_tool(&tmp.path, "bash", r#""override":true"#);
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        let names = mgr.overridden_tool_names();
        assert!(names.contains("bash"));

        // override:true on a NON-built-in name → not in overridden set (there's
        // nothing to override; it's just a custom tool).
        let tmp2 = TmpDir::new("override_custom");
        write_plugin_with_tool(&tmp2.path, "my_domain_tool", r#""override":true"#);
        let mgr2 = PluginManager::new(tmp2.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr2.overridden_tool_names().is_empty());

        // A plain collision (no override) on a built-in → NOT overridden.
        let tmp3 = TmpDir::new("override_none");
        write_plugin_with_tool(&tmp3.path, "read_file", r#""kind":"readonly""#);
        let mgr3 = PluginManager::new(tmp3.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr3.overridden_tool_names().is_empty());
    }

    #[test]
    fn system_prompt_injection_concat_and_framed() {
        let tmp = TmpDir::new("sysprompt_inject");
        let a = tmp.path.join("a");
        let b = tmp.path.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        write_manifest(
            &a,
            r#"{"name":"alpha","version":"1.0.0","system_prompt":"rule A"}"#,
        );
        write_manifest(
            &b,
            r#"{"name":"beta","version":"2.0.0","system_prompt":"rule B"}"#,
        );
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        let inj = mgr.system_prompt_injection();
        assert!(inj.starts_with("\n\n## Plugin-injected context\n\n"));
        assert!(inj.contains("# Plugin: alpha (v1.0.0)\nrule A"));
        assert!(inj.contains("# Plugin: beta (v2.0.0)\nrule B"));

        // Empty when no plugin declares one (prefix-cache-safe).
        let tmp2 = TmpDir::new("sysprompt_empty");
        write_manifest(&tmp2.path, r#"{"name":"plain","version":"1.0.0"}"#);
        let mgr2 = PluginManager::new(tmp2.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr2.system_prompt_injection().is_empty());
    }

    #[test]
    fn has_hook_existence_check() {
        let tmp = TmpDir::new("has_hook");
        // PluginManager::new scans SUBDIRECTORIES of its root for plugins,
        // so the plugin must live in a subdir.
        let pdir = tmp.path.join("h");
        let hooks_dir = pdir.join("hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        write_hook_script(&hooks_dir, "h.sh", r#"{"allow":true}"#, 0);
        write_manifest(
            &pdir,
            r#"{"name":"h","version":"1.0.0","hooks":{"pre_tool":{"script":"hooks/h.sh"}}}"#,
        );
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr.has_hook("pre_tool"));
        assert!(!mgr.has_hook("pre_bash"));
        // Disabled plugin is excluded.
        mgr.disable("h").unwrap();
        assert!(!mgr.has_hook("pre_tool"));
    }

    #[test]
    fn disabled_plugin_excluded_from_new_capabilities() {
        // A disabled plugin contributes nothing to disable_tools / overrides /
        // system_prompt — mirroring how disabled plugins are excluded from
        // hook configs and tool definitions.
        let tmp = TmpDir::new("disabled_excluded");
        let pdir = write_plugin_with_tool(&tmp.path, "bash", r#""override":true"#);
        // Augment with disable_tools + system_prompt.
        let manifest = fs::read_to_string(pdir.join("plugin.json")).unwrap();
        let manifest = manifest.trim_end_matches('}').to_string()
            + r#","disable_tools":["edit"],"system_prompt":"ctx"}"#;
        fs::write(pdir.join("plugin.json"), &manifest).unwrap();

        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/__t_ws__"), true);
        assert!(mgr.overridden_tool_names().contains("bash"));
        assert!(mgr.disabled_tools().contains("edit"));
        assert!(mgr.system_prompt_injection().contains("ctx"));

        mgr.disable("tools-plugin").unwrap();
        assert!(mgr.overridden_tool_names().is_empty());
        assert!(mgr.disabled_tools().is_empty());
        assert!(mgr.system_prompt_injection().is_empty());
    }

    fn tool_config_for(script: PathBuf, timeout_ms: u64, kind: ToolKind) -> ToolConfig {
        ToolConfig {
            name: "ut".into(),
            description: "".into(),
            parameters: json!({}),
            script,
            timeout_ms,
            kind,
            override_builtin: false,
            plugin_name: "test-plugin".into(),
        }
    }

    #[tokio::test]
    async fn execute_plugin_tool_json_output() {
        let tmp = TmpDir::new("ept_json");
        let script = write_hook_script(&tmp.path, "t.sh", r#"{"ok":true,"output":"hi there"}"#, 0);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({"x":1}), "/ws", "s.jsonl").await;
        assert!(out.ok);
        assert_eq!(out.output, "hi there");
    }

    #[tokio::test]
    async fn execute_plugin_tool_raw_output() {
        let tmp = TmpDir::new("ept_raw");
        let script = write_hook_script(&tmp.path, "t.sh", "plain result", 0);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(out.ok);
        assert_eq!(out.output, "plain result");
    }

    #[tokio::test]
    async fn execute_plugin_tool_error_output() {
        let tmp = TmpDir::new("ept_err");
        let script = write_hook_script(&tmp.path, "t.sh", r#"{"ok":false,"output":"boom"}"#, 0);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(!out.ok);
        assert_eq!(out.output, "boom");
    }

    #[tokio::test]
    async fn execute_plugin_tool_error_field() {
        let tmp = TmpDir::new("ept_errfield");
        let script = write_hook_script(&tmp.path, "t.sh", r#"{"error":"something failed"}"#, 0);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(!out.ok);
        assert!(out.output.contains("something failed"));
    }

    #[tokio::test]
    async fn execute_plugin_tool_nonzero_exit() {
        let tmp = TmpDir::new("ept_exit");
        let script = write_hook_script(&tmp.path, "t.sh", r#"{"ok":true,"output":"x"}"#, 3);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(!out.ok);
        assert!(out.output.contains("exited"));
    }

    #[tokio::test]
    async fn execute_plugin_tool_timeout() {
        let tmp = TmpDir::new("ept_timeout");
        // Handler sleeps well past the tool's timeout.
        let script = tmp.path.join("slow.sh");
        fs::write(
            &script,
            "#!/bin/sh\nsleep 2\necho '{\"ok\":true,\"output\":\"late\"}'\n",
        )
        .unwrap();
        make_executable(&script);
        let tc = tool_config_for(script, 300, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(!out.ok);
        assert!(out.output.contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_plugin_tool_cancellation_kills_child() {
        let tmp = TmpDir::new("ept_cancel");
        let started = tmp.path.join("started");
        let completed = tmp.path.join("completed");
        let script = tmp.path.join("cancel.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nsleep 1\ntouch '{}'\necho '{{\"ok\":true,\"output\":\"late\"}}'\n",
                started.display(),
                completed.display()
            ),
        )
        .unwrap();
        make_executable(&script);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let args = json!({});
        let mut future = Box::pin(execute_plugin_tool("ut", &tc, &args, "/ws", "s.jsonl"));
        let started_result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(1), async {
                while !started.exists() {
                    tokio::task::yield_now().await;
                }
            }) => result,
            _ = &mut future => panic!("plugin completed before cancellation"),
        };
        started_result.expect("plugin child did not start");
        drop(future);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(
            !completed.exists(),
            "dropping the plugin future must kill its child before completion"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_plugin_tool_stops_reading_at_output_limit() {
        let tmp = TmpDir::new("ept_oversized");
        let script = tmp.path.join("oversized.sh");
        fs::write(
            &script,
            "#!/bin/sh\ndd if=/dev/zero bs=1048577 count=1 2>/dev/null\n",
        )
        .unwrap();
        make_executable(&script);
        let tc = tool_config_for(script, 5000, ToolKind::Destructive);
        let out = execute_plugin_tool("ut", &tc, &json!({}), "/ws", "s.jsonl").await;
        assert!(!out.ok);
        assert!(out.output.contains("exceeds the 1048576 byte limit"));
    }

    // ---- prompt-backed command (agent_turn) tests ----

    fn write_prompt_file(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_prompt_backed_command() {
        let tmp = TmpDir::new("load_prompt_cmd");
        write_prompt_file(&tmp.path, "prompts/research.md", "Research: {{args}}");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [
    { "name": "deep-research", "description": "Deep research", "prompt_file": "prompts/research.md" }
  ]
}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        assert_eq!(plugin.commands.len(), 1);
        let c = &plugin.commands[0];
        assert_eq!(c.name, "deep-research");
        assert_eq!(c.mode, CommandMode::AgentTurn);
        assert!(c.script.is_none());
        let pf = c.prompt_file.as_ref().expect("prompt_file set");
        assert!(pf.ends_with("prompts/research.md"));
    }

    #[test]
    fn load_rejects_missing_prompt_file() {
        let tmp = TmpDir::new("missing_prompt");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "prompts/missing.md" } ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn load_rejects_prompt_file_traversal() {
        let tmp = TmpDir::new("traversal_prompt");
        write_prompt_file(&tmp.path, "prompts/ok.md", "x");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "../escape.md" } ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("escapes"), "{err}");
    }

    #[test]
    fn load_rejects_absolute_prompt_file() {
        let tmp = TmpDir::new("abs_prompt");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "/etc/passwd" } ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("must be relative"), "{err}");
    }

    #[test]
    fn load_rejects_both_script_and_prompt_file() {
        let tmp = TmpDir::new("both_cmd");
        let scripts = tmp.path.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        write_hook_script(&scripts, "greet.sh", r#"{"ok":true}"#, 0);
        write_prompt_file(&tmp.path, "prompts/p.md", "x");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [
    { "name": "deep-research", "script": "scripts/greet.sh", "prompt_file": "prompts/p.md" }
  ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("both"), "{err}");
    }

    #[test]
    fn load_rejects_neither_script_nor_prompt_file() {
        let tmp = TmpDir::new("neither_cmd");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research" } ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("neither"), "{err}");
    }

    #[test]
    fn load_rejects_mode_mismatch() {
        let tmp = TmpDir::new("mode_mismatch");
        write_prompt_file(&tmp.path, "prompts/p.md", "x");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [
    { "name": "deep-research", "prompt_file": "prompts/p.md", "mode": "script" }
  ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("mode="), "{err}");
    }

    #[test]
    fn load_rejects_oversized_prompt_file() {
        let tmp = TmpDir::new("oversize_prompt");
        let big = "a".repeat(MAX_PROMPT_COMMAND_BYTES + 1);
        write_prompt_file(&tmp.path, "prompts/big.md", &big);
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "prompts/big.md" } ]
}"#,
        );
        let err = PluginManager::load_plugin_from_dir(&tmp.path).unwrap_err();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn script_command_still_loads_as_script_mode() {
        // Backward compatibility: a script-backed command loads with mode=Script.
        let tmp = TmpDir::new("script_compat");
        let scripts = tmp.path.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        write_hook_script(&scripts, "greet.sh", r#"{"ok":true,"output":"hi"}"#, 0);
        write_manifest(
            &tmp.path,
            r#"{
  "name": "cmd",
  "version": "1.0.0",
  "commands": [ { "name": "greet", "script": "scripts/greet.sh" } ]
}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let c = &plugin.commands[0];
        assert_eq!(c.mode, CommandMode::Script);
        assert!(c.script.is_some());
        assert!(c.prompt_file.is_none());
    }

    #[test]
    fn command_definitions_include_mode() {
        // PluginManager scans a plugins *directory* for plugin subdirs, so the
        // plugin must live in a subdir of the tmp plugins dir (not directly).
        let tmp = TmpDir::new("cmd_defs_mode");
        let plugin_dir = tmp.path.join("dr");
        let scripts = plugin_dir.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        write_hook_script(&scripts, "greet.sh", r#"{"ok":true}"#, 0);
        write_prompt_file(&plugin_dir, "prompts/p.md", "Research {{args}}");
        write_manifest(
            &plugin_dir,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [
    { "name": "greet", "script": "scripts/greet.sh" },
    { "name": "deep-research", "prompt_file": "prompts/p.md" }
  ]
}"#,
        );
        let mgr = PluginManager::new(tmp.path.clone(), PathBuf::from("/ws"), true);
        let defs = mgr.command_definitions();
        let greet = defs.iter().find(|d| d["name"] == "greet").expect("greet");
        assert_eq!(greet["mode"], "script");
        let dr = defs
            .iter()
            .find(|d| d["name"] == "deep-research")
            .expect("deep-research");
        assert_eq!(dr["mode"], "agent_turn");
    }

    #[test]
    fn render_command_template_substitutes_all_tokens() {
        let tmpl = "cmd={{command}} plugin={{plugin}} ws={{workspace}} sid={{session_id}} ts={{timestamp}} args=[{{args}}] spaced={{ args }}";
        let out = render_command_template(
            tmpl,
            "query",
            "/ws",
            "s.jsonl",
            1700000000,
            "dr",
            "deep-research",
        );
        assert!(out.contains("cmd=deep-research"));
        assert!(out.contains("plugin=dr"));
        assert!(out.contains("ws=/ws"));
        assert!(out.contains("sid=s.jsonl"));
        assert!(out.contains("ts=1700000000"));
        assert!(out.contains("args=[query]"));
        assert!(out.contains("spaced=query"));
    }

    #[test]
    fn render_command_template_leaves_unknown_tokens_verbatim() {
        let tmpl = "keep {{unknown}} and {{not_a_token}} as-is";
        let out = render_command_template(tmpl, "a", "w", "s", 1, "p", "c");
        assert!(out.contains("{{unknown}}"));
        assert!(out.contains("{{not_a_token}}"));
    }

    #[test]
    fn render_prompt_command_reads_file_and_substitutes() {
        let tmp = TmpDir::new("render_prompt_cmd");
        write_prompt_file(
            &tmp.path,
            "prompts/r.md",
            "Research request: {{args}}\nWorkspace: {{workspace}}",
        );
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "prompts/r.md" } ]
}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let cfg = &plugin.commands[0];
        let rendered =
            render_prompt_command(cfg, "compare vLLM vs SGLang", "/ws", "s.jsonl").unwrap();
        assert!(rendered.contains("Research request: compare vLLM vs SGLang"));
        assert!(rendered.contains("Workspace: /ws"));
    }

    #[test]
    fn render_prompt_command_empty_args_substitutes_empty() {
        // Empty args is allowed at render time; the dispatcher emits a usage
        // message before starting the turn. The template substitutes "".
        let tmp = TmpDir::new("render_empty_args");
        write_prompt_file(&tmp.path, "prompts/r.md", "Request: [{{args}}]");
        write_manifest(
            &tmp.path,
            r#"{
  "name": "dr",
  "version": "1.0.0",
  "commands": [ { "name": "deep-research", "prompt_file": "prompts/r.md" } ]
}"#,
        );
        let plugin = PluginManager::load_plugin_from_dir(&tmp.path).unwrap();
        let cfg = &plugin.commands[0];
        let rendered = render_prompt_command(cfg, "", "/ws", "s.jsonl").unwrap();
        assert_eq!(rendered, "Request: []");
    }

    #[test]
    fn bundled_deep_research_plugin_loads_as_agent_turn() {
        // Integration test: the real deep-research plugin shipped in the repo
        // must load with two prompt-backed (agent_turn) commands and a valid,
        // path-confined prompt_file. Locates the plugin relative to the crate.
        let plugin_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join(".catalyst-code/plugins/deep-research");
        if !plugin_dir.join("plugin.json").exists() {
            eprintln!("skipped: deep-research plugin not found at {plugin_dir:?}");
            return;
        }
        let plugin = PluginManager::load_plugin_from_dir(&plugin_dir)
            .unwrap_or_else(|e| panic!("deep-research plugin failed to load: {e}"));
        assert_eq!(plugin.name, "deep-research");
        assert_eq!(
            plugin.commands.len(),
            2,
            "expected deep-research + research alias"
        );
        for c in &plugin.commands {
            assert_eq!(
                c.mode,
                CommandMode::AgentTurn,
                "command {} not agent_turn",
                c.name
            );
            assert!(
                c.script.is_none(),
                "command {} should have no script",
                c.name
            );
            let pf = c.prompt_file.as_ref().expect("prompt_file set");
            assert!(pf.ends_with("prompts/deep-research.md"));
            assert!(pf.starts_with(&plugin_dir.canonicalize().unwrap()));
        }
        // The alias and the primary command share the same prompt file.
        let dr = plugin
            .commands
            .iter()
            .find(|c| c.name == "deep-research")
            .unwrap();
        let alias = plugin
            .commands
            .iter()
            .find(|c| c.name == "research")
            .unwrap();
        assert_eq!(dr.prompt_file, alias.prompt_file);
        // Rendering the real prompt with sample args must succeed and embed the args.
        let rendered = render_prompt_command(dr, "compare vLLM vs SGLang", "/ws", "s.jsonl")
            .unwrap_or_else(|e| panic!("render failed: {e}"));
        assert!(rendered.contains("compare vLLM vs SGLang"));
        assert!(rendered.contains("deep-research"));
    }
}
