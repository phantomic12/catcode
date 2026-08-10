use crate::config::Config;
use crate::tools::{smart_truncate, Outcome};
use serde_json::Value;

fn git_exec(cfg: &Config, subcmd: &[&str]) -> Outcome {
    use std::io::Read;
    fn read_all<R: Read>(r: &mut R) -> String {
        let mut v = Vec::new();
        let _ = r.read_to_end(&mut v);
        String::from_utf8_lossy(&v).into_owned()
    }
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(&cfg.workspace)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .args(subcmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Outcome::err("git not found on PATH");
        }
        Err(e) => return Outcome::err(format!("git exec failed: {e}")),
    };
    let out_h = child.stdout.take();
    let err_h = child.stderr.take();
    let t_out = std::thread::spawn(move || out_h.map(|mut r| read_all(&mut r)).unwrap_or_default());
    let t_err = std::thread::spawn(move || err_h.map(|mut r| read_all(&mut r)).unwrap_or_default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(_) => break None,
        }
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    // Same 32KB smart_truncate CAP as bash — git log/diff dumps can blow context.
    const CAP: usize = 32_768;
    let cap = |body: String| -> String {
        if body.len() > CAP {
            smart_truncate(&body, CAP)
        } else {
            body
        }
    };
    match status {
        Some(s) if s.success() => {
            let body = if !stdout.trim().is_empty() {
                stdout
            } else if !stderr.trim().is_empty() {
                stderr
            } else {
                String::from("(no output)")
            };
            Outcome::ok(cap(body))
        }
        Some(s) => {
            let body = if !stderr.trim().is_empty() {
                stderr
            } else if !stdout.trim().is_empty() {
                stdout
            } else {
                format!("git {:?} failed (exit {:?})", subcmd, s.code())
            };
            Outcome::err(cap(body))
        }
        None => Outcome::err(format!("git {:?} timed out after 30s", subcmd)),
    }
}

/// Validate a workspace-relative git pathspec: reject absolute paths and `..`
/// escapes. Returns Ok("") for an empty path (meaning "no path filter").
fn git_rel_path(p: &str) -> Result<String, String> {
    if p.is_empty() {
        return Ok(String::new());
    }
    if p.starts_with('/') || p.starts_with('\\') || (p.len() >= 2 && p.as_bytes()[1] == b':') {
        return Err(format!(
            "git path must be workspace-relative, got absolute: {p:?}"
        ));
    }
    for comp in p.split(['/', '\\']) {
        if comp == ".." {
            return Err(format!(
                "git path must not escape the workspace (..): {p:?}"
            ));
        }
    }
    Ok(p.to_string())
}

pub(crate) fn git_status(args: &Value, cfg: &Config) -> Outcome {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match git_rel_path(path) {
        Err(e) => Outcome::err(e),
        Ok(p) if p.is_empty() => git_exec(cfg, &["status", "--short", "--branch"]),
        Ok(p) => git_exec(cfg, &["status", "--short", "--branch", "--", &p]),
    }
}

pub(crate) fn git_diff(args: &Value, cfg: &Config) -> Outcome {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let staged = args
        .get("staged")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match git_rel_path(path) {
        Err(e) => Outcome::err(e),
        Ok(p) => {
            let mut cmd: Vec<String> = vec!["diff".into(), "--no-color".into()];
            if staged {
                cmd.push("--staged".into());
            }
            if !p.is_empty() {
                cmd.push("--".into());
                cmd.push(p);
            }
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            git_exec(cfg, &refs)
        }
    }
}

pub(crate) fn git_log(args: &Value, cfg: &Config) -> Outcome {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(1000);
    match git_rel_path(path) {
        Err(e) => Outcome::err(e),
        Ok(p) => {
            let n = format!("-n{limit}");
            let mut cmd: Vec<String> = vec!["log".into(), "--oneline".into(), n];
            if !p.is_empty() {
                cmd.push("--".into());
                cmd.push(p);
            }
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            git_exec(cfg, &refs)
        }
    }
}

/// List OTHER active catalyst-code sessions in this workspace (separate
/// processes), each with its goal, in-progress work, and recently touched
/// files. Awareness only — read-only broadcast of "who is here". Use when
/// something seems off to decide whether a neighbor caused it before assuming
/// you introduced the error. Stale/crashed sessions are auto-pruned by mtime.
pub(crate) fn workspace_activity(_args: &Value, cfg: &Config) -> Outcome {
    let my_pid = std::process::id();
    let peers = crate::presence::read_peers(&cfg.workspace, my_pid);
    if peers.is_empty() {
        return Outcome::ok(
            "No other active catalyst-code sessions in this workspace. Any error \
             you are seeing is from your own work or the environment.",
        );
    }
    let now = crate::presence::unix_now();
    let mut out = format!(
        "{} other active session(s) in this workspace:\n",
        peers.len()
    );
    for p in &peers {
        out.push_str(&format!(
            "\n- pid {} (started {}, last active {})",
            p.pid,
            age(now, p.started_at),
            age(now, p.last_heartbeat)
        ));
        if let Some(sid) = &p.session_id {
            out.push_str(&format!(", session {sid}"));
        }
        if let Some(m) = &p.model {
            out.push_str(&format!(", model {m}"));
        }
        if !p.goal.is_empty() {
            out.push_str(&format!("\n  goal: {}", truncate(p.goal.as_str(), 140)));
        }
        if !p.in_progress.is_empty() {
            out.push_str(&format!("\n  in progress: {}", p.in_progress.join("; ")));
        }
        if !p.next.is_empty() {
            out.push_str(&format!("\n  next: {}", p.next.join("; ")));
        }
        if !p.recent_files.is_empty() {
            out.push_str(&format!(
                "\n  recently touched: {}",
                p.recent_files.join(", ")
            ));
        }
        if !p.last_activity.is_empty() {
            out.push_str(&format!(
                "\n  last: {}",
                truncate(p.last_activity.as_str(), 140)
            ));
        }
    }
    Outcome::ok(out)
}

/// Render a unix-seconds delta as a compact human age ("3m", "2h", "just now").
fn age(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    if s < 5 {
        "just now".to_string()
    } else if s < 60 {
        format!("{}s ago", s)
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

/// Truncate `s` to at most `n` chars, appending an ellipsis if cut. A small
/// local copy of main.rs's `truncate_str` (kept private there) so this module
/// stays self-contained.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub(crate) fn git_add(args: &Value, cfg: &Config) -> Outcome {
    let Some(paths) = args.get("paths").and_then(|v| v.as_array()) else {
        return Outcome::err("git_add requires a 'paths' array");
    };
    if paths.is_empty() {
        return Outcome::err("git_add requires a non-empty 'paths' array");
    }
    let mut cmd: Vec<String> = vec!["add".into(), "--".into()];
    for p in paths {
        let Some(ps) = p.as_str() else {
            return Outcome::err("git_add: every path must be a string");
        };
        match git_rel_path(ps) {
            Err(e) => return Outcome::err(e),
            Ok(rp) => cmd.push(rp),
        }
    }
    let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    git_exec(cfg, &refs)
}

pub(crate) fn git_commit(args: &Value, cfg: &Config) -> Outcome {
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    if message.trim().is_empty() {
        return Outcome::err("git_commit requires a non-empty 'message'");
    }
    let mut cmd: Vec<String> = vec!["commit".into(), "-m".into(), message.to_string()];
    if all {
        cmd.push("--all".into());
    }
    let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    git_exec(cfg, &refs)
}

/// Validate a git object / ref / remote / branch token: no whitespace or shell
/// metacharacters. Empty is allowed only when `allow_empty`.
fn git_token(label: &str, raw: &str, allow_empty: bool) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return if allow_empty {
            Ok(String::new())
        } else {
            Err(format!("{label} must be non-empty"))
        };
    }
    if t.starts_with('-') {
        return Err(format!("{label} must not look like a flag: {t:?}"));
    }
    if t.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                ';' | '|' | '&' | '`' | '$' | '(' | ')' | '<' | '>' | '\n' | '\r' | '\0'
            )
    }) {
        return Err(format!("{label} contains illegal characters: {t:?}"));
    }
    Ok(t.to_string())
}

/// `git show <object> [-- <path>]` — commit/blob/tree contents. Prefer over bash
/// `git show`. Read-only.
pub(crate) fn git_show(args: &Value, cfg: &Config) -> Outcome {
    let object = args.get("object").and_then(|v| v.as_str()).unwrap_or("");
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let object = match git_token("object", object, false) {
        Ok(o) => o,
        Err(e) => return Outcome::err(e),
    };
    match git_rel_path(path) {
        Err(e) => Outcome::err(e),
        Ok(p) => {
            let mut cmd: Vec<String> = vec!["show".into(), "--no-color".into(), object];
            if !p.is_empty() {
                cmd.push("--".into());
                cmd.push(p);
            }
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            git_exec(cfg, &refs)
        }
    }
}

/// `git push [<remote>] [<refspec>]` with optional `-u` / `--tags`.
pub(crate) fn git_push(args: &Value, cfg: &Config) -> Outcome {
    let remote = args
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin");
    let refspec = args.get("refspec").and_then(|v| v.as_str()).unwrap_or("");
    let set_upstream = args
        .get("set_upstream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let tags = args.get("tags").and_then(|v| v.as_bool()).unwrap_or(false);
    let remote = match git_token("remote", remote, false) {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    let refspec = match git_token("refspec", refspec, true) {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    let mut cmd: Vec<String> = vec!["push".into()];
    if set_upstream {
        cmd.push("-u".into());
    }
    if tags {
        cmd.push("--tags".into());
    }
    cmd.push(remote);
    if !refspec.is_empty() {
        cmd.push(refspec);
    }
    let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    git_exec(cfg, &refs)
}

/// `git pull [<remote>] [<refspec>]` with optional `--rebase`.
pub(crate) fn git_pull(args: &Value, cfg: &Config) -> Outcome {
    let remote = args
        .get("remote")
        .and_then(|v| v.as_str())
        .unwrap_or("origin");
    let refspec = args.get("refspec").and_then(|v| v.as_str()).unwrap_or("");
    let rebase = args
        .get("rebase")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let remote = match git_token("remote", remote, false) {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    let refspec = match git_token("refspec", refspec, true) {
        Ok(r) => r,
        Err(e) => return Outcome::err(e),
    };
    let mut cmd: Vec<String> = vec!["pull".into()];
    if rebase {
        cmd.push("--rebase".into());
    }
    cmd.push(remote);
    if !refspec.is_empty() {
        cmd.push(refspec);
    }
    let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    git_exec(cfg, &refs)
}

/// Branch list / create / delete / checkout. Prefer over bash `git branch`.
pub(crate) fn git_branch(args: &Value, cfg: &Config) -> Outcome {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("list");
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
    match action {
        "list" => {
            let mut cmd: Vec<String> = vec!["branch".into(), "--list".into(), "-v".into()];
            if all {
                cmd.push("-a".into());
            }
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            git_exec(cfg, &refs)
        }
        "create" | "delete" | "checkout" => {
            let name = match git_token("name", name, false) {
                Ok(n) => n,
                Err(e) => return Outcome::err(e),
            };
            let cmd: Vec<String> = match action {
                "create" => vec!["branch".into(), name],
                "delete" => vec!["branch".into(), "-d".into(), name],
                "checkout" => vec!["switch".into(), name],
                _ => unreachable!(),
            };
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            git_exec(cfg, &refs)
        }
        other => Outcome::err(format!(
            "git_branch action must be list|create|delete|checkout, got {other:?}"
        )),
    }
}

// ---- sandboxed (microVM) git path ----
// When sandboxing is enabled, built-in git runs inside the microVM via the
// shared execution backend (never directly on the host). The argv is built by
// the same validation as the host path (workspace-relative pathspecs, no `..`),
// so command input cannot reach outside the mounted workspace.

/// Build the full git argv (program `git` + subcommand + validated args) shared
/// by the host and sandboxed paths. Returns `Err` for invalid args.
pub(crate) fn git_argv(name: &str, args: &Value) -> Result<Vec<String>, String> {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "git_status" => {
            let p = git_rel_path(path)?;
            let mut v = vec![
                "git".to_string(),
                "status".into(),
                "--short".into(),
                "--branch".into(),
            ];
            if !p.is_empty() {
                v.push("--".into());
                v.push(p);
            }
            Ok(v)
        }
        "git_diff" => {
            let staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let p = git_rel_path(path)?;
            let mut v = vec!["git".to_string(), "diff".into(), "--no-color".into()];
            if staged {
                v.push("--staged".into());
            }
            if !p.is_empty() {
                v.push("--".into());
                v.push(p);
            }
            Ok(v)
        }
        "git_log" => {
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(20)
                .min(1000);
            let p = git_rel_path(path)?;
            let mut v = vec![
                "git".to_string(),
                "log".into(),
                "--oneline".into(),
                format!("-n{limit}"),
            ];
            if !p.is_empty() {
                v.push("--".into());
                v.push(p);
            }
            Ok(v)
        }
        "git_add" => {
            let Some(paths) = args.get("paths").and_then(|v| v.as_array()) else {
                return Err("git_add requires a 'paths' array".into());
            };
            if paths.is_empty() {
                return Err("git_add requires a non-empty 'paths' array".into());
            }
            let mut v = vec!["git".to_string(), "add".into(), "--".into()];
            for p in paths {
                let Some(ps) = p.as_str() else {
                    return Err("git_add: every path must be a string".into());
                };
                v.push(git_rel_path(ps)?);
            }
            Ok(v)
        }
        "git_commit" => {
            let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            if message.trim().is_empty() {
                return Err("git_commit requires a non-empty 'message'".into());
            }
            let mut v = vec![
                "git".to_string(),
                "commit".into(),
                "-m".into(),
                message.to_string(),
            ];
            if all {
                v.push("--all".into());
            }
            Ok(v)
        }
        "git_show" => {
            let object = git_token(
                "object",
                args.get("object").and_then(|v| v.as_str()).unwrap_or(""),
                false,
            )?;
            let p = git_rel_path(path)?;
            let mut v = vec![
                "git".to_string(),
                "show".into(),
                "--no-color".into(),
                object,
            ];
            if !p.is_empty() {
                v.push("--".into());
                v.push(p);
            }
            Ok(v)
        }
        "git_push" => {
            let remote = git_token(
                "remote",
                args.get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin"),
                false,
            )?;
            let refspec = git_token(
                "refspec",
                args.get("refspec").and_then(|v| v.as_str()).unwrap_or(""),
                true,
            )?;
            let set_upstream = args
                .get("set_upstream")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let tags = args.get("tags").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut v = vec!["git".to_string(), "push".into()];
            if set_upstream {
                v.push("-u".into());
            }
            if tags {
                v.push("--tags".into());
            }
            v.push(remote);
            if !refspec.is_empty() {
                v.push(refspec);
            }
            Ok(v)
        }
        "git_pull" => {
            let remote = git_token(
                "remote",
                args.get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin"),
                false,
            )?;
            let refspec = git_token(
                "refspec",
                args.get("refspec").and_then(|v| v.as_str()).unwrap_or(""),
                true,
            )?;
            let rebase = args
                .get("rebase")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut v = vec!["git".to_string(), "pull".into()];
            if rebase {
                v.push("--rebase".into());
            }
            v.push(remote);
            if !refspec.is_empty() {
                v.push(refspec);
            }
            Ok(v)
        }
        "git_branch" => {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("list");
            let all = args.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
            match action {
                "list" => {
                    let mut v = vec![
                        "git".to_string(),
                        "branch".into(),
                        "--list".into(),
                        "-v".into(),
                    ];
                    if all {
                        v.push("-a".into());
                    }
                    Ok(v)
                }
                "create" | "delete" | "checkout" => {
                    let name = git_token(
                        "name",
                        args.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        false,
                    )?;
                    Ok(match action {
                        "create" => vec!["git".into(), "branch".into(), name],
                        "delete" => vec!["git".into(), "branch".into(), "-d".into(), name],
                        "checkout" => vec!["git".into(), "switch".into(), name],
                        _ => unreachable!(),
                    })
                }
                other => Err(format!(
                    "git_branch action must be list|create|delete|checkout, got {other:?}"
                )),
            }
        }
        other => Err(format!("unknown git tool: {other}")),
    }
}

/// Async sandboxed git execution: runs the validated argv inside the active
/// sandbox via the shared execution backend. GIT_PAGER/PAGER/HOME are set in
/// the guest base env; git identity comes from explicit CatCode settings, never
/// from the host's ~/.gitconfig or ~/.ssh.
pub(crate) async fn git_dispatch(name: &str, args: &Value, cfg: &Config) -> Outcome {
    let argv = match git_argv(name, args) {
        Ok(a) => a,
        Err(e) => return Outcome::err(e),
    };
    let proc_env =
        crate::sandbox::policy::build_process_env(cfg, crate::sandbox::policy::ExecPurpose::Git);
    let cwd =
        crate::sandbox::policy::effective_cwd(cfg, "").unwrap_or_else(|_| cfg.workspace.clone());
    let req = crate::sandbox::ExecRequest {
        program: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd,
        env: proc_env.env,
        inherit_parent_env: proc_env.inherit_parent,
        stdin: None,
        timeout: std::time::Duration::from_secs(30),
        ..Default::default()
    };
    match crate::sandbox::execution_backend().execute(req).await {
        Ok(r) => {
            let stdout = String::from_utf8_lossy(&r.stdout);
            let stderr = String::from_utf8_lossy(&r.stderr);
            let body = if !stdout.trim().is_empty() {
                stdout.into_owned()
            } else if !stderr.trim().is_empty() {
                stderr.into_owned()
            } else {
                "(no output)".to_string()
            };
            const CAP: usize = 32_768;
            let body = if body.len() > CAP {
                crate::tools::smart_truncate(&body, CAP)
            } else {
                body
            };
            if r.exit_code == Some(0) {
                Outcome::ok(body)
            } else {
                Outcome::err(body)
            }
        }
        Err(e) => Outcome::err(e.user_message()),
    }
}

// ---- memory tool (agent-callable wrapper over crate::memory) ----
