//! Sandbox policy: guest environment construction, secret denial, network-mode
//! translation, shell selection, and workspace path confinement.
//!
//! These are the security invariants the task demands:
//!   - No host environment inheritance (minimal guest env, secrets denied).
//!   - `Approval::Never` must NOT disable workspace file confinement.
//!   - Workspace mapped to `/workspace`; no host home / `.ssh` / sockets.
//!   - `--no-network` enforced through Microsandbox network policy.
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::Config;

/// Which shell the `bash` tool runs commands in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellKind {
    /// POSIX `bash -c <command>` (Unix host, or any host when sandboxed — the
    /// guest is Linux).
    Posix,
    /// Windows PowerShell (`-NoProfile -NonInteractive -Command`).
    PowerShell,
    /// Windows Command Prompt (`cmd.exe /d /c <command>`).
    Cmd,
}

impl ShellKind {
    pub fn is_posix(self) -> bool {
        matches!(self, ShellKind::Posix)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Posix => "bash",
            ShellKind::PowerShell => "powershell",
            ShellKind::Cmd => "cmd",
        }
    }
}

/// Classify a shell program path/name into a [`ShellKind`].
///
/// Accepts bare names (`bash`, `pwsh`, `cmd`) and full paths including
/// Windows-style paths with backslashes — even when running on a Unix host
/// (e.g. unit tests, or a mis-set `CATALYST_CODE_SHELL`).
fn shell_kind_for_program(prog: &str) -> ShellKind {
    let stem = shell_program_stem(prog);
    match stem.as_str() {
        "bash" | "sh" | "zsh" | "dash" | "ksh" | "ash" | "busybox" => ShellKind::Posix,
        "cmd" => ShellKind::Cmd,
        // Default non-POSIX host shells (powershell / pwsh / unknown) use the
        // PowerShell argv form. Callers that need strict validation should
        // check the stem explicitly.
        _ => ShellKind::PowerShell,
    }
}

/// Basename stem of a shell program, tolerant of `/` and `\\` separators and
/// optional `.exe` (so `C:\\...\\bash.exe` classifies as `bash` on any OS).
fn shell_program_stem(prog: &str) -> String {
    let trimmed = prog.trim().trim_matches('"').trim_matches('\'');
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name);
    stem.to_ascii_lowercase()
}

/// Read whether sandboxing is enabled from the active backend (single source of
/// truth — the global backend is set at startup from config).
pub fn is_sandbox_enabled() -> bool {
    super::execution_backend().is_sandboxed()
}

/// The effective shell kind for the `bash` tool. When sandboxed, the guest is
/// always Linux `bash` (POSIX), so Windows users are no longer told to emit
/// PowerShell. When unsandboxed, it follows the host-native shell.
pub fn effective_shell_kind() -> ShellKind {
    if is_sandbox_enabled() {
        return ShellKind::Posix;
    }
    host_shell_kind()
}

/// Host-native shell kind (ignores sandbox state). Mirrors `tools::shell_is_posix`.
fn host_shell_kind() -> ShellKind {
    shell_kind_for_program(&resolve_host_shell())
}

/// Resolve the host shell program (CATALYST_CODE_SHELL override or OS default).
/// Honors full paths (e.g. `C:\\Program Files\\Git\\bin\\bash.exe`,
/// `%COMSPEC%`, `pwsh`).
pub(crate) fn resolve_host_shell() -> String {
    if let Ok(s) = std::env::var("CATALYST_CODE_SHELL") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    #[cfg(target_os = "windows")]
    {
        if crate::tools::pwsh_available() {
            "pwsh".to_string()
        } else {
            "powershell".to_string()
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        "bash".to_string()
    }
}

/// Prefer PowerShell Core (`pwsh`) when present, else Windows PowerShell.
/// Used by plugin `.ps1` hooks so they match the bash-tool default.
pub(crate) fn resolve_powershell_program() -> String {
    #[cfg(target_os = "windows")]
    {
        if crate::tools::pwsh_available() {
            "pwsh".to_string()
        } else {
            "powershell".to_string()
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Cross-platform PowerShell Core name; spawn fails cleanly if absent.
        "pwsh".to_string()
    }
}

/// Build `(program, args)` for a concrete shell program path/name.
///
/// - POSIX stems (`bash`/`sh`/…): `<prog> -c <command>`
/// - `cmd` / `cmd.exe`: `<prog> /d /c <command>`
/// - PowerShell (`powershell`/`pwsh`): `-NoProfile -NonInteractive -Command`
pub fn shell_argv_for_program(prog: &str, command: &str) -> (String, Vec<String>) {
    match shell_kind_for_program(prog) {
        ShellKind::Posix => (
            prog.to_string(),
            vec!["-c".to_string(), command.to_string()],
        ),
        ShellKind::Cmd => (
            prog.to_string(),
            vec!["/d".to_string(), "/c".to_string(), command.to_string()],
        ),
        ShellKind::PowerShell => (
            prog.to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        ),
    }
}

/// Build `(program, args)` to run a single command string in the active shell.
/// POSIX: `<shell> -c <command>`. PowerShell: `-NoProfile -NonInteractive
/// -Command`. Cmd: `/d /c`. When sandboxed the program is always `bash`
/// (resolved in the guest), never a Windows host shell path.
pub fn shell_argv(command: &str) -> (String, Vec<String>) {
    if is_sandbox_enabled() {
        return (
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        );
    }
    let prog = resolve_host_shell();
    shell_argv_for_program(&prog, command)
}

/// Purpose of an exec — selects which host env extras (compiler caches) to
/// include when running unsandboxed. Under the microVM the guest image owns its
/// own toolchains, so no host extras are forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecPurpose {
    Bash,
    Diagnostics,
    Git,
    Plugin,
}

impl ExecPurpose {
    fn host_extras(&self) -> &'static [&'static str] {
        match self {
            ExecPurpose::Diagnostics => &[
                "CARGO_HOME",
                "RUSTUP_HOME",
                "GOPATH",
                "GOCACHE",
                "GOTMPDIR",
                "NODE_PATH",
                "npm_config_cache",
            ],
            _ => &[],
        }
    }
}

/// Result of env preparation: the env map to apply plus whether the host
/// backend should inherit the parent environment (Windows PowerShell path).
#[derive(Clone, Debug, Default)]
pub struct ProcessEnv {
    pub env: BTreeMap<String, String>,
    pub inherit_parent: bool,
}

/// Build the per-command environment for the active backend.
///
/// - **Host, POSIX shell:** `env_clear` + PATH/HOME/TMPDIR/USER (+ purpose
///   extras). No LD_PRELOAD / proxy leak.
/// - **Host, PowerShell / cmd:** inherit the parent env (SystemRoot/PATHEXT/
///   APPDATA/COMSPEC are required); apply nothing extra.
/// - **Microsandbox:** empty per-command map — the base guest env is set at
///   sandbox creation (see [`guest_base_env`]); secrets are never inherited.
pub fn build_process_env(cfg: &Config, purpose: ExecPurpose) -> ProcessEnv {
    if is_sandbox_enabled() {
        return ProcessEnv::default();
    }
    let kind = host_shell_kind();
    if !kind.is_posix() {
        // Windows shells depend on the full process environment.
        return ProcessEnv {
            env: BTreeMap::new(),
            inherit_parent: true,
        };
    }
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".to_string(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
    );
    if let Ok(home) = std::env::var("HOME") {
        env.insert("HOME".into(), home);
    }
    if let Ok(tmp) = std::env::var("TMPDIR") {
        env.insert("TMPDIR".into(), tmp);
    }
    if let Ok(user) = std::env::var("USER") {
        env.insert("USER".into(), user);
    }
    for k in purpose.host_extras() {
        if let Ok(v) = std::env::var(k) {
            env.insert((*k).into(), v);
        }
    }
    let _ = cfg;
    ProcessEnv {
        env,
        inherit_parent: false,
    }
}

/// The minimal guest environment baked into every Microsandbox sandbox at
/// creation. Deliberately does NOT inherit the host environment. Additional
/// variables may be added via `sandbox_env_allowlist` (after secret filtering).
pub fn guest_base_env(cfg: &Config) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
    );
    env.insert("HOME".into(), "/home/catcode".into());
    env.insert("LANG".into(), "C.UTF-8".into());
    env.insert("LC_ALL".into(), "C.UTF-8".into());
    env.insert("TERM".into(), "dumb".into());
    if std::env::var("CI").is_ok() {
        env.insert("CI".into(), "1".into());
    }
    env.insert("CATCODE_SANDBOX".into(), "1".into());
    env.insert("CATCODE_WORKSPACE".into(), "/workspace".into());
    env.insert("GIT_PAGER".into(), "cat".into());
    env.insert("PAGER".into(), "cat".into());
    // Explicitly-allowlisted host vars (secrets denied even if listed here).
    for name in &cfg.sandbox_env_allowlist {
        if let Ok(val) = std::env::var(name) {
            if !is_secret_var(name) {
                env.insert(name.clone(), val);
            }
        }
    }
    env
}

/// Patterns of environment variables that always carry secrets and must never be
/// forwarded to the guest, even if present in `sandbox_env_allowlist`.
const SECRET_PATTERNS: &[&str] = &[
    "_TOKEN",
    "_SECRET",
    "_PASSWORD",
    "_API_KEY",
    "_APIKEY",
    "_CREDENTIAL",
    "_KEY",
    "_PASS",
];

/// Whether a variable name matches a secret-bearing pattern. Conservative:
/// matches suffixes like `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `*_API_KEY`, plus
/// well-known cloud/provider/agent-secret names.
pub fn is_secret_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    for suf in SECRET_PATTERNS {
        if upper.ends_with(suf) {
            return true;
        }
    }
    matches!(
        upper.as_str(),
        "AWS_ACCESS_KEY_ID"
            | "AWS_SECRET_ACCESS_KEY"
            | "AWS_SESSION_TOKEN"
            | "AZURE_CLIENT_SECRET"
            | "AZURE_TENANT_ID"
            | "AZURE_SUBSCRIPTION_ID"
            | "GOOGLE_APPLICATION_CREDENTIALS"
            | "GOOGLE_CLOUD_PROJECT"
            | "GITHUB_TOKEN"
            | "GH_TOKEN"
            | "NPM_TOKEN"
            | "NPM_AUTHTOKEN"
            | "PYPI_TOKEN"
            | "SSH_AUTH_SOCK"
            | "DOCKER_HOST"
            | "DOCKER_CONFIG"
            | "KUBECONFIG"
            | "PGPASSWORD"
            | "PGUSER"
            | "MYSQL_PWD"
            | "DATABASE_URL"
            | "REDIS_URL"
            | "OPENAI_APIKEY"
            | "OPENAI_API_KEY"
            | "ANTHROPIC_API_KEY"
            | "TOKEN"
            | "SECRET"
            | "PASSWORD"
    )
}

/// Workspace mount point inside the guest.
pub const GUEST_WORKSPACE: &str = "/workspace";

/// Translate a workspace-relative path (`""` = workspace root) into the cwd for
/// the active backend. Sandboxed → `/workspace[/rel]`; host →
/// `cfg.workspace[/rel]`. Absolute host paths and `..` escapes are rejected so
/// command input cannot mount/arbitrary-access host paths via the guest.
pub fn effective_cwd(cfg: &Config, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Ok(if is_sandbox_enabled() {
            PathBuf::from(GUEST_WORKSPACE)
        } else {
            cfg.workspace.clone()
        });
    }
    // Reject absolute paths and Windows drive letters — they would let a command
    // reach outside the mounted workspace.
    if rel.starts_with('/')
        || rel.starts_with('\\')
        || (rel.len() >= 2 && rel.as_bytes()[1] == b':')
    {
        return Err(format!(
            "path must be workspace-relative, got absolute: {rel:?}"
        ));
    }
    for comp in rel.split(['/', '\\']) {
        if comp == ".." {
            return Err(format!("path must not escape the workspace (..): {rel:?}"));
        }
    }
    if is_sandbox_enabled() {
        Ok(PathBuf::from(GUEST_WORKSPACE).join(rel))
    } else {
        Ok(cfg.workspace.join(rel))
    }
}

/// Resolve a host workspace path to its guest equivalent (used when a caller
/// The model-facing description of the `bash` tool for the active shell. When
/// sandboxed, Windows users are told the guest is Linux `bash` (not PowerShell).
pub fn bash_tool_description() -> &'static str {
    if is_sandbox_enabled() {
        return "Run a bash command inside the sandbox microVM (Linux guest). The workspace is mounted at /workspace; stdout+stderr are captured, truncated to 32KB, default 30s timeout. Pass timeout for slow builds. Keep commands short; for complex logic write a script with write_file and run `bash script.sh`. Do not use for git status/diff/log/show, grep/rg, cat/sed -n, ls, or find when native tools apply. The environment is isolated: host secrets and the host home directory are not available.";
    }
    match effective_shell_kind() {
        ShellKind::Posix => "Run a bash command in the workspace (stdout+stderr, truncated to 32KB, default 30s timeout). Pass timeout for slow builds. Keep commands short; for complex logic write a script with write_file and run bash script.sh. Do not use for git status/diff/log/show, grep/rg, cat/sed -n, ls, or find when native tools apply.",
        ShellKind::PowerShell => "Run a shell command in the workspace (PowerShell; stdout+stderr, truncated to 32KB, default 30s timeout). Pass timeout for slow builds. Keep commands short; for complex logic write a .ps1 script with write_file and run `powershell -File script.ps1`.",
        ShellKind::Cmd => "Run a shell command in the workspace (cmd.exe; stdout+stderr, truncated to 32KB, default 30s timeout). Pass timeout for slow builds. Keep commands short; for complex logic write a .cmd/.bat script with write_file and run it via `cmd /c script.cmd`. Use cmd.exe syntax (`%VAR%`, `&&`, `dir`), not PowerShell.",
    }
}

/// OS-/sandbox-aware shell guidance for the standing system prompt so the model
/// emits syntax that matches the live `bash` tool (not a compile-time OS guess).
pub fn shell_guidance() -> &'static str {
    if is_sandbox_enabled() {
        return "Shell: the `bash` tool runs commands in Linux bash inside the sandbox microVM. Write POSIX syntax. For complex logic write a script with write_file and run `bash script.sh`.";
    }
    match effective_shell_kind() {
        ShellKind::Posix => {
            "Shell: the `bash` tool runs commands in bash. For complex logic write a script with write_file and run `bash script.sh`."
        }
        ShellKind::PowerShell => {
            "Shell: the `bash` tool runs commands in PowerShell (pwsh if installed, else Windows PowerShell). Write PowerShell syntax — e.g. `Get-ChildItem`/`gci`, `Select-String`, `Remove-Item`, `$env:VAR`, `$LASTEXITCODE`. For complex logic write a `.ps1` script with write_file and run `powershell -File script.ps1`. Avoid POSIX-isms (`&&`/`||` chains, `2>/dev/null`, `export`); use `;`/`if`/`$()`/`$env:` instead."
        }
        ShellKind::Cmd => {
            "Shell: the `bash` tool runs commands in cmd.exe. Write cmd syntax — e.g. `dir`, `type`, `findstr`, `%VAR%`, `&&`/`||`, `if errorlevel`. For complex logic write a `.cmd` script with write_file and run `cmd /c script.cmd`. Avoid PowerShell-only cmdlets and POSIX-only constructs."
        }
    }
}

#[cfg(test)]
mod shell_argv_tests {
    use super::*;

    #[test]
    fn shell_kind_classifies_common_stems() {
        assert_eq!(shell_kind_for_program("bash"), ShellKind::Posix);
        assert_eq!(
            shell_kind_for_program(r"C:\Program Files\Git\bin\bash.exe"),
            ShellKind::Posix
        );
        assert_eq!(shell_kind_for_program("pwsh"), ShellKind::PowerShell);
        assert_eq!(
            shell_kind_for_program("powershell.exe"),
            ShellKind::PowerShell
        );
        assert_eq!(shell_kind_for_program("cmd"), ShellKind::Cmd);
        assert_eq!(
            shell_kind_for_program(r"C:\Windows\System32\cmd.exe"),
            ShellKind::Cmd
        );
    }

    #[test]
    fn shell_argv_for_program_uses_matching_flags() {
        let (p, a) = shell_argv_for_program("bash", "echo hi");
        assert_eq!(p, "bash");
        assert_eq!(a, vec!["-c".to_string(), "echo hi".to_string()]);

        let (p, a) = shell_argv_for_program("pwsh", "Write-Output hi");
        assert_eq!(p, "pwsh");
        assert_eq!(
            a,
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                "Write-Output hi".to_string(),
            ]
        );

        let (p, a) = shell_argv_for_program("cmd.exe", "echo hi");
        assert_eq!(p, "cmd.exe");
        assert_eq!(
            a,
            vec!["/d".to_string(), "/c".to_string(), "echo hi".to_string()]
        );
    }
}
