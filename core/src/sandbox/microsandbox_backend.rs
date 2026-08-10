//! The Microsandbox execution backend — the ONLY place that touches the
//! `microsandbox` SDK. Feature-gated behind `microsandbox`; the rest of the
//! codebase depends only on the [`ExecutionBackend`](super::backend) trait.
//!
//! Lazily creates and reuses one microVM per (workspace, session). The VM is
//! booted on the first agent-controlled exec and kept alive for the session so
//! package installs / build caches persist. On an unhealthy execution the VM is
//! reset (stopped + recreated) rather than silently reusing a broken one.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::{Config, SandboxNetworkMode};
use crate::sandbox::backend::{ExecRequest, ExecResult, ExecutionBackend};
use crate::sandbox::error::{
    error_codes, CheckStatus, ExecutionError, SandboxPreflightCheck, SandboxPreflightReport,
};
use crate::sandbox::policy::{guest_base_env, GUEST_WORKSPACE};
use crate::sandbox::preflight::{run_platform_preflight, RealProbe};

use microsandbox::sandbox::Sandbox;
const TRUNCATION_MARKER: &[u8] = b"...[truncated]...\n";

fn append_bounded(tail: &mut Vec<u8>, chunk: &[u8], max: usize, truncated: &mut bool) {
    if max == 0 {
        *truncated |= !chunk.is_empty();
        return;
    }
    if chunk.len() >= max {
        tail.clear();
        tail.extend_from_slice(&chunk[chunk.len() - max..]);
        *truncated = true;
        return;
    }
    let overflow = tail.len().saturating_add(chunk.len()).saturating_sub(max);
    if overflow > 0 {
        tail.drain(..overflow);
        *truncated = true;
    }
    tail.extend_from_slice(chunk);
}

fn finalize_bounded(tail: Vec<u8>, max: usize, truncated: bool) -> Vec<u8> {
    if !truncated {
        return tail;
    }
    let mut output = Vec::with_capacity(TRUNCATION_MARKER.len().saturating_add(max));
    output.extend_from_slice(TRUNCATION_MARKER);
    output.extend_from_slice(&tail);
    output
}

/// A reused Microsandbox microVM.
pub struct MicrosandboxExecutionBackend {
    cfg: Arc<Config>,
    workspace: PathBuf,
    name: String,
    sandbox: tokio::sync::Mutex<Option<Sandbox>>,
    /// Set when an execution looked unhealthy; the next exec recreates the VM.
    unhealthy: std::sync::atomic::AtomicBool,
    last_status: tokio::sync::Mutex<Option<SandboxPreflightReport>>,
}

impl MicrosandboxExecutionBackend {
    pub fn new(cfg: Arc<Config>) -> Self {
        let workspace = cfg.workspace.clone();
        let name = sandbox_name(&workspace);
        Self {
            cfg,
            workspace,
            name,
            sandbox: tokio::sync::Mutex::new(None),
            unhealthy: std::sync::atomic::AtomicBool::new(false),
            last_status: tokio::sync::Mutex::new(None),
        }
    }

    /// Lazily create (or reuse) the VM. Returns a cloned [`Sandbox`] (Arc-backed,
    /// cheap). On any readiness/boot failure returns [`ExecutionError`] — the
    /// caller MUST NOT fall back to the host.
    async fn ensure_sandbox(&self) -> Result<Sandbox, ExecutionError> {
        // Fast path: reuse a healthy VM.
        {
            let guard = self.sandbox.lock().await;
            if let Some(sb) = guard.as_ref() {
                if !self.unhealthy.load(std::sync::atomic::Ordering::Relaxed) {
                    return Ok(sb.clone());
                }
            }
        }
        // Need to (re)create. Drop any stale VM first.
        self.teardown_inner().await;

        // 1. Platform preflight (no SDK needed).
        let report = run_platform_preflight(&RealProbe, true);
        if !report.ready {
            *self.last_status.lock().await = Some(report.clone());
            return Err(ExecutionError::SetupRequired { report });
        }
        // 2. Runtime assets (msb + libkrunfw) installed?
        let mut report = report;
        if !microsandbox::setup::is_installed() {
            report.checks.push(SandboxPreflightCheck {
                code: error_codes::RUNTIME_MISSING.to_string(),
                title: "Microsandbox runtime".to_string(),
                status: CheckStatus::Fail,
                detail: "runtime assets not installed (msb + libkrunfw)".to_string(),
            });
            report.ready = false;
            report.actions.push(crate::sandbox::preflight::action(
                "Prepare Microsandbox runtime",
                "CatCode needs to download the Microsandbox runtime (msb + libkrunfw) once. Run `/sandbox setup` or `prepare_sandbox` to download verified assets into the CatCode cache.",
                None,
                false,
                false,
            ));
            *self.last_status.lock().await = Some(report.clone());
            return Err(ExecutionError::SetupRequired { report });
        }
        report.checks.push(SandboxPreflightCheck {
            code: "runtime".to_string(),
            title: "Microsandbox runtime".to_string(),
            status: CheckStatus::Pass,
            detail: "installed".to_string(),
        });

        // 3. Build + create the VM (image pull happens here).
        let sb = match self.build_and_create(&report).await {
            Ok(sb) => sb,
            Err(e) => {
                let code = classify_create_error(&e.to_string());
                report.checks.push(SandboxPreflightCheck {
                    code: code.to_string(),
                    title: "Sandbox creation".to_string(),
                    status: CheckStatus::Fail,
                    detail: e.to_string(),
                });
                report.ready = false;
                *self.last_status.lock().await = Some(report.clone());
                return Err(ExecutionError::Sandbox {
                    code: code.to_string(),
                    message: e.to_string(),
                });
            }
        };
        report.checks.push(SandboxPreflightCheck {
            code: "sandbox".to_string(),
            title: "Sandbox".to_string(),
            status: CheckStatus::Pass,
            detail: format!("running (image {})", self.cfg.sandbox_image),
        });
        report.ready = true;
        *self.last_status.lock().await = Some(report);
        self.unhealthy
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mut guard = self.sandbox.lock().await;
        *guard = Some(sb.clone());
        Ok(sb)
    }

    /// Build the SandboxBuilder with the configured image/resources/env/network
    /// and create+boot the VM.
    async fn build_and_create(&self, _report: &SandboxPreflightReport) -> Result<Sandbox, String> {
        use microsandbox::Sandbox;

        let mut builder = Sandbox::builder(self.name.clone())
            .image(self.cfg.sandbox_image.clone())
            .cpus(self.cfg.sandbox_cpus)
            .memory(self.cfg.sandbox_memory_mb)
            .oci_upper_size(self.cfg.sandbox_disk_mb)
            .workdir(GUEST_WORKSPACE)
            .hostname("catcode".to_string())
            .replace();

        // Workspace mount: host workspace -> /workspace (writable).
        let ws = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());
        builder = builder.volume(GUEST_WORKSPACE.to_string(), |m| m.bind(ws.clone()));

        // Global plugin dir (read-only) so user-installed plugin scripts run in
        // the guest without exposing the whole ~/.catalyst-code config dir.
        // This must match PluginManager::new_with_global_plugins, not
        // cfg.plugin_dir (which names the project plugin directory).
        if let Some(plugin_dir) = crate::config::home_dir()
            .map(|home| home.join(".catalyst-code/plugins"))
            .filter(|dir| dir.exists())
        {
            let plugin_dir = plugin_dir.canonicalize().unwrap_or(plugin_dir);
            builder = builder.volume("/catcode-plugins".to_string(), |m| {
                m.bind(plugin_dir).readonly()
            });
        }

        // Guest env (minimal; secrets denied inside guest_base_env).
        let env = guest_base_env(&self.cfg);
        builder = builder.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        // Network policy.
        builder = self.apply_network(builder);

        builder.create().await.map_err(|e| e.to_string())
    }

    /// Apply the configured network mode to the builder.
    fn apply_network(
        &self,
        builder: microsandbox::sandbox::SandboxBuilder,
    ) -> microsandbox::sandbox::SandboxBuilder {
        match self.cfg.sandbox_network_mode {
            SandboxNetworkMode::None => builder.disable_network(),
            SandboxNetworkMode::Restricted => {
                if self.cfg.sandbox_allow_private_networks {
                    builder.network(|n| n.policy(microsandbox::NetworkPolicy::non_local()))
                } else {
                    builder.network(|n| n.policy(microsandbox::NetworkPolicy::public_only()))
                }
            }
            SandboxNetworkMode::Allowlist => {
                let entries: Vec<String> = self.cfg.sandbox_network_allowlist.clone();
                let (domains, suffixes): (Vec<String>, Vec<String>) = entries
                    .into_iter()
                    .partition(|e| !e.starts_with('.') && !e.contains('/'));
                match microsandbox::NetworkPolicy::builder()
                    .default_deny()
                    .egress(|r| {
                        // DNS is resolved by the SDK's built-in interceptor;
                        // only the allowlisted domains/suffixes (plus optionally
                        // private ranges) are permitted for application egress.
                        r.allow_domains(domains.iter());
                        r.allow_domain_suffixes(suffixes.iter());
                        if self.cfg.sandbox_allow_private_networks {
                            r.allow_private();
                        }
                        r
                    })
                    .build()
                {
                    Ok(policy) => builder.network(|n| n.policy(policy)),
                    // Fail closed: a broken allowlist must NOT upgrade to public
                    // egress (CORE_REVIEW HIGH).
                    Err(e) => {
                        eprintln!(
                            "[sandbox] network allowlist policy failed to build ({e}); disabling network"
                        );
                        builder.disable_network()
                    }
                }
            }
        }
    }

    async fn teardown_inner(&self) {
        let mut guard = self.sandbox.lock().await;
        if let Some(sb) = guard.take() {
            let _ = sb.stop_with_timeout(Duration::from_secs(5)).await;
        }
    }

    /// Map an SDK exec failure (binary missing, guest agent down, …) to a
    /// CatCode error. Binary-missing becomes an actionable MissingExecutable.
    fn map_exec_error(&self, e: &microsandbox::MicrosandboxError) -> ExecutionError {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        // ExecFailed carries "not found"/"permission denied" for the guest binary.
        if lower.contains("not found")
            || lower.contains("no such file")
            || lower.contains("not executable")
        {
            return ExecutionError::missing_executable(
                &self.guess_program(),
                &self.cfg.sandbox_image,
            );
        }
        ExecutionError::Sandbox {
            code: error_codes::GUEST_AGENT_UNAVAILABLE.to_string(),
            message: msg,
        }
    }

    fn guess_program(&self) -> String {
        // Best-effort: the last request's program. (Caller passes it via the
        // request; we don't store it, so report a generic label.)
        "command".to_string()
    }
}

#[async_trait]
impl ExecutionBackend for MicrosandboxExecutionBackend {
    fn is_sandboxed(&self) -> bool {
        true
    }

    async fn execute(&self, request: ExecRequest) -> Result<ExecResult, ExecutionError> {
        let sb = self.ensure_sandbox().await?;

        // Build exec options. The program runs in the guest; cwd/env are guest.
        let cwd = request.cwd.display().to_string();
        let program = request.program.clone();
        let args = request.args.clone();
        let env_iter: Vec<(String, String)> = request
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let timeout = request.timeout;
        let max_out = request.max_stdout_bytes;
        let max_err = request.max_stderr_bytes;
        let stdin_bytes = request.stdin.clone();

        let mut handle = match sb
            .exec_stream_with(program.clone(), move |b| {
                // Configure the builder the SDK hands us — args, guest cwd,
                // guest env, timeout (SIGKILL on expiry), and stdin payload.
                // Returning `b` unmodified would run the program with default
                // options: no args/cwd/env/timeout/stdin.
                let mut b = b
                    .args(args.iter().map(|a| a.as_str()))
                    .cwd(cwd.as_str())
                    .envs(env_iter.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .timeout(timeout);
                b = match &stdin_bytes {
                    Some(data) => b.stdin_bytes(data.clone()),
                    None => b.stdin_null(),
                };
                b
            })
            .await
        {
            Ok(h) => h,
            Err(e) => {
                // Spawn failure in the guest — likely a missing executable.
                self.unhealthy
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return Err(self.map_exec_error(&e));
            }
        };

        let control = handle.control();
        let mut stdout = Vec::with_capacity(max_out.min(8 * 1024));
        let mut stderr = Vec::with_capacity(max_err.min(8 * 1024));
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        let collect = async {
            let mut exit_code = None;
            while let Some(event) = handle.recv().await {
                match event {
                    microsandbox::ExecEvent::Stdout(data) => {
                        append_bounded(&mut stdout, data.as_ref(), max_out, &mut stdout_truncated);
                    }
                    microsandbox::ExecEvent::Stderr(data) => {
                        append_bounded(&mut stderr, data.as_ref(), max_err, &mut stderr_truncated);
                    }
                    microsandbox::ExecEvent::Exited { code } => {
                        exit_code = Some(code);
                        break;
                    }
                    microsandbox::ExecEvent::Failed(e) => {
                        return Err(microsandbox::MicrosandboxError::ExecFailed(e))
                    }
                    microsandbox::ExecEvent::Started { .. }
                    | microsandbox::ExecEvent::StdinError(_) => {}
                }
            }
            let code = exit_code.ok_or_else(|| {
                microsandbox::MicrosandboxError::Runtime(
                    "exec session ended without exit event".into(),
                )
            })?;
            Ok((code, stdout, stderr, stdout_truncated, stderr_truncated))
        };
        match tokio::time::timeout(timeout, collect).await {
            Ok(Ok((code, stdout, stderr, stdout_truncated, stderr_truncated))) => Ok(ExecResult {
                exit_code: Some(code),
                stdout: finalize_bounded(stdout, max_out, stdout_truncated),
                stderr: finalize_bounded(stderr, max_err, stderr_truncated),
                timed_out: false,
            }),
            Ok(Err(e)) => {
                let err = self.map_exec_error(&e);
                if !matches!(err, ExecutionError::MissingExecutable { .. }) {
                    self.unhealthy
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(err)
            }
            Err(_) => {
                let _ = control.kill().await;
                Ok(ExecResult {
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    timed_out: true,
                })
            }
        }
    }

    async fn reset(&self) -> Result<(), ExecutionError> {
        self.unhealthy
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.teardown_inner().await;
        Ok(())
    }

    async fn prepare(&self) -> Result<(), ExecutionError> {
        // Install into the SDK default base dir (`~/.microsandbox` / `$MSB_HOME`).
        // `is_installed()` and msb resolution only look there — a custom CatCode
        // cache path made prepare "succeed" while every exec still reported
        // runtime_missing (CORE_REVIEW C6 / microsandbox-auto-setup).
        if let Err(e) = microsandbox::setup::Setup::builder()
            .build()
            .install()
            .await
        {
            let mut report = run_platform_preflight(&RealProbe, true);
            report.checks.push(SandboxPreflightCheck {
                code: error_codes::RUNTIME_DOWNLOAD_FAILED.to_string(),
                title: "Runtime download".to_string(),
                status: CheckStatus::Fail,
                detail: e.to_string(),
            });
            report.ready = false;
            return Err(ExecutionError::SetupRequired { report });
        }
        Ok(())
    }

    async fn status(&self) -> SandboxPreflightReport {
        if let Some(r) = self.last_status.lock().await.clone() {
            return r;
        }
        run_platform_preflight(&RealProbe, true)
    }

    async fn shutdown(&self) {
        self.teardown_inner().await;
    }
}

/// Derive a valid sandbox name from the workspace path + PID.
fn sandbox_name(workspace: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in workspace.display().to_string().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let pid = std::process::id();
    let raw = format!("catcode-{:x}-{}", h, pid);
    // Sanitize to the SDK's name rules: start alphanumeric, only
    // alphanumeric/dot/hyphen/underscore, ≤128 bytes.
    let mut name = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            name.push(ch);
        } else if name.is_empty() {
            name.push('x');
        } else {
            name.push('-');
        }
    }
    if name.is_empty() || !name.chars().next().unwrap_or('x').is_ascii_alphanumeric() {
        name.insert(0, 'c');
    }
    if name.len() > 120 {
        name.truncate(120);
    }
    name
}

/// Where CatCode stores Microsandbox runtime assets (NOT ~/.microsandbox).
#[allow(dead_code)]
fn cache_dir() -> PathBuf {
    if let Some(c) = dirs_cache() {
        c.join("microsandbox")
    } else {
        PathBuf::from(".cache").join("microsandbox")
    }
}

#[allow(dead_code)]
fn dirs_cache() -> Option<PathBuf> {
    std::env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .map(|p| p.join("catalyst-code"))
}

fn classify_create_error(msg: &str) -> &'static str {
    let l = msg.to_ascii_lowercase();
    if l.contains("image")
        && (l.contains("pull") || l.contains("not found") || l.contains("manifest"))
    {
        error_codes::IMAGE_PULL_FAILED
    } else if l.contains("boot") || l.contains("kvm") || l.contains("permission") {
        error_codes::SANDBOX_BOOT_FAILED
    } else if l.contains("already exists") {
        error_codes::SANDBOX_BOOT_FAILED
    } else {
        error_codes::SANDBOX_BOOT_FAILED
    }
}
