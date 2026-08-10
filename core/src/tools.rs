// Built-in tools the agent can call. OpenAI function-calling schema.
// All file ops are confined to the workspace root; bash runs with cwd=workspace
// and a real timeout+kill. read_file returns plain content; edit uses search/replace.
use crate::config::{Approval, Config};
use crate::tooling::builtin::git::{
    git_add, git_commit, git_diff, git_log, git_status, workspace_activity,
};
use crate::tooling::builtin::memory::{knowledge_tool, memory_tool};
use crate::workspace;
use serde_json::{json, Value};

pub use crate::fetch_tool::execute_fetch;
pub use crate::search_tool::execute_web_search;
pub use crate::test_env::execute_test_env;

/// Read a bounded resource through the deferred `runtime` group. Workspace
/// paths retain the exact paging semantics of `read_file`; every other scheme
/// is resolved by an owned store rather than passed to a shell.
pub async fn execute_unified_read(
    args: &Value,
    cfg: &Config,
    session_file: Option<&std::path::Path>,
) -> Outcome {
    const ARCHIVE_ENTRY_MAX: u64 = 2 * 1024 * 1024;
    const ARCHIVE_MAX_ENTRIES: usize = 256;

    let Some(uri) = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Outcome::err("read requires string field 'path'");
    };
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return crate::fetch_tool::execute_fetch(&json!({"url": uri}), cfg).await;
    }
    if let Some(name) = uri.strip_prefix("skill://") {
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
            return Outcome::err("read: unsafe skill URI");
        }
        return crate::subagent::discover_skills_full(&cfg.workspace)
            .into_iter()
            .find(|skill| skill.name == name)
            .map(|skill| Outcome::ok(skill.body))
            .unwrap_or_else(|| Outcome::err(format!("read: skill '{name}' was not found")));
    }
    if let Some(id) = uri.strip_prefix("memory://") {
        if id.is_empty() || id.contains('/') || id.contains('\\') || id.contains("..") {
            return Outcome::err("read: unsafe memory URI");
        }
        return crate::memory::get_memory(&cfg.workspace, id)
            .map(|memory| Outcome::ok(memory.content))
            .unwrap_or_else(|e| Outcome::err(format!("read: memory '{id}' unavailable: {e}")));
    }
    if let Some(run_id) = uri.strip_prefix("artifact://") {
        if run_id.is_empty()
            || run_id.len() > 128
            || !run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Outcome::err("read: unsafe artifact URI");
        }
        let Some(session_file) = session_file else {
            return Outcome::err(
                "read: artifact resolution is unavailable outside an owned session",
            );
        };
        return crate::session::read_job_artifact(session_file, run_id)
            .map(|artifact| {
                Outcome::ok(smart_truncate(
                    &artifact.to_string(),
                    cfg.max_read_bytes as usize,
                ))
            })
            .unwrap_or_else(|| {
                Outcome::err(format!("read: owned artifact '{run_id}' was not found"))
            });
    }
    if let Some((archive, entry)) = uri.split_once("!/") {
        if entry.is_empty()
            || entry.starts_with('/')
            || entry.contains('\\')
            || entry.split('/').any(|part| part == "..")
        {
            return Outcome::err("read: unsafe archive entry path");
        }
        let archive_path = match resolve_ws(cfg, archive) {
            Ok(path) => path,
            Err(e) => return Outcome::err(e),
        };
        let file = match std::fs::File::open(&archive_path) {
            Ok(file) => file,
            Err(e) => return Outcome::err(format!("read: archive {archive:?} failed: {e}")),
        };
        let mut zip = match zip::ZipArchive::new(file) {
            Ok(zip) => zip,
            Err(e) => {
                return Outcome::err(format!(
                    "read: {archive:?} is not a supported zip archive: {e}"
                ))
            }
        };
        if zip.len() > ARCHIVE_MAX_ENTRIES {
            return Outcome::err(format!(
                "read: archive has {} entries (max {ARCHIVE_MAX_ENTRIES})",
                zip.len()
            ));
        }
        let mut member = match zip.by_name(entry) {
            Ok(member) => member,
            Err(_) => return Outcome::err(format!("read: archive entry {entry:?} was not found")),
        };
        if member.is_dir()
            || member.size() > ARCHIVE_ENTRY_MAX
            || member.size() > cfg.max_read_bytes
        {
            return Outcome::err(format!(
                "read: archive entry exceeds {} byte limit",
                ARCHIVE_ENTRY_MAX.min(cfg.max_read_bytes)
            ));
        }
        use std::io::Read;
        let mut body = String::with_capacity(member.size() as usize);
        if let Err(e) = member.read_to_string(&mut body) {
            return Outcome::err(format!("read: archive entry is not UTF-8 text: {e}"));
        }
        return Outcome::ok(body);
    }
    read_file(uri, args, cfg)
}

/// Description shown to the model for the `bash` tool. OS-selected so the
/// model emits matching syntax: PowerShell on Windows, bash on Unix. The
/// Model-facing description of the `bash` tool. When sandboxing is enabled the
/// guest is always Linux `bash`, so Windows users are no longer told to emit
/// PowerShell. Delegates to the sandbox policy (single source of truth).
pub(crate) fn bash_tool_desc() -> &'static str {
    crate::sandbox::policy::bash_tool_description()
}

pub use crate::tooling::policy::{classify, is_parallel_wave_tool};
pub use crate::tooling::scheduler::execute_parallel_wave;
pub use crate::tooling::ToolKind;

/// Internal sentinel returned by the `finish` tool. The orchestrator treats this
/// as loop exit; the UI/session see [`FINISH_MESSAGE`] instead.
pub const FINISH_SENTINEL: &str = "__finish__";

/// Human-readable tool_result shown when the agent calls `finish`.
pub const FINISH_MESSAGE: &str = "This turn has finished";

/// Tools always included in the main agent's request schema (cheap, high-use).
pub use crate::tooling::schema::{
    deferred_tool_names, definitions, is_builtin, is_core_tool, is_deferred_tool,
};
/// Outcome of a tool call. For bash we need a future with timeout+kill, so
/// destructive/bash execution is split: execute() handles sync tools;
/// execute_bash() is async and takes a runtime handle.
#[derive(Clone)]
pub struct Outcome {
    pub ok: bool,
    pub output: String,
    /// Optional unified-diff rendering of the change (edit/patch/write_file).
    /// Surfaced to the TUI as a separate `diff` event field so the model's
    /// tool-result content (output) stays compact — the diff is for humans.
    pub diff: Option<String>,
}

/// Execute a (non-bash) tool call synchronously. `cfg` provides confinement+limits.
/// bash is handled separately by execute_bash (async, timeout+kill).
pub fn execute(name: &str, args: &Value, cfg: &Config) -> Outcome {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "read_file" => read_file(s("path"), args, cfg),
        "todo_read" => todo_read(cfg),
        "todo_write" => todo_write(args, cfg),
        // Sentinel stays internal; main.rs / subagent map it to a human-readable
        // tool_result ("This turn has finished") before emitting to the UI.
        "finish" => Outcome::ok(FINISH_SENTINEL),
        "patch" => apply_patch(args, cfg),
        "diagnostics" => Outcome::err("diagnostics must be dispatched through execute_diagnostics (async)"),
        "fetch" => Outcome::err("fetch must be dispatched through execute_fetch (async)"),
        "web_search" => Outcome::err("web_search must be dispatched through execute_web_search (async)"),
        name if crate::browser::is_browser_tool(name) => Outcome::err("browser tools must be dispatched through execute_browser (async)"),
        "spawn" | "subagent" => Outcome::err("subagent must be dispatched through execute_subagent (async)"),
        "contact_supervisor" | "intercom" => Outcome::err("intercom tools must be dispatched through execute_intercom (async, subagent context only)"),
        "ask" => Outcome::err("ask must be dispatched through request_ask (async, orchestrator loop only)"),
        "load_tools" => Outcome::err(
            "load_tools must be dispatched through handle_load_tools (orchestrator loop only)",
        ),
        "edit" => {
            let path = s("path");
            match args.get("edits").and_then(|v| v.as_array()) {
                Some(e) if !e.is_empty() => execute_edit(path, e, cfg),
                _ => Outcome::err("edit requires a non-empty 'edits' array"),
            }
        }
        "write_file" => {
            let path = s("path");
            // Require the `content` key (schema marks it required). Missing key
            // used to silently write "" and truncate existing files (H5).
            // Explicit empty string is allowed when the key is present.
            match require_string_arg(args, "content") {
                Ok(content) => write_file(path, content, cfg),
                Err(e) => Outcome::err(e),
            }
        }
        "delete" => delete_path(s("path"), cfg),
        "rename" => rename_path(s("from"), s("to"), cfg),
        "mkdir" => mkdir_path(s("path"), cfg),
        "list_dir" => list_dir(s("path"), cfg),
        "grep" => grep(s("pattern"), args, cfg),
        "glob" => glob(s("pattern"), cfg),
        "bulk_read" => bulk_read(args, cfg),
        "bulk_write" => bulk_write(args, cfg),
        "bulk_edit" => bulk_edit(args, cfg),
        "git_status" => git_status(args, cfg),
        "git_diff" => git_diff(args, cfg),
        "git_log" => git_log(args, cfg),
        "workspace_activity" => workspace_activity(args, cfg),
        "git_add" => git_add(args, cfg),
        "git_commit" => git_commit(args, cfg),
        "memory" => memory_tool(args, cfg),
        "knowledge" => knowledge_tool(args, cfg),
        "collections" => collections_tool(args, cfg),
        "goal_write_plan" => Outcome::err(
            "goal_write_plan must be dispatched through handle_goal_write_plan (async, goal mode only)",
        ),
        "snapshot_edit" => crate::tooling::ide::execute_snapshot_edit(args, cfg),
        "ast_edit" => Outcome::err("ast_edit must be dispatched asynchronously"),
        "bulk" => Outcome::err("bulk must be dispatched through execute_bulk (async)"),
        other => Outcome::err(format!("unknown tool: {other}")),
    }
}

impl Outcome {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self {
            ok: true,
            output: msg.into(),
            diff: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: msg.into(),
            diff: None,
        }
    }
}

/// Require a string argument key to be present. Null or non-string values fail.
/// Empty string is allowed when the key exists (explicit wipe).
fn require_string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.as_str()),
        Some(_) => Err(format!("'{key}' must be a string")),
        None => Err(format!("missing required '{key}'")),
    }
}

// ---- file tools ----

/// Resolve a tool path against the workspace root. Approval::Never means
/// approval-free host access only when the host backend is active; an enabled
/// sandbox always preserves workspace confinement so file tools cannot bypass
/// the guest boundary and reach arbitrary host paths.
fn resolve_ws(cfg: &Config, input: &str) -> Result<std::path::PathBuf, String> {
    if matches!(cfg.approval, Approval::Never) && !crate::sandbox::is_sandbox_enabled() {
        workspace::resolve_unconfined(&cfg.workspace, input)
    } else {
        workspace::resolve(&cfg.workspace, input)
    }
}

fn read_file(input: &str, args: &Value, cfg: &Config) -> Outcome {
    let path = match resolve_ws(cfg, input) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Outcome::err(format!("read_file {input:?} failed: {e}")),
    };
    if meta.len() > cfg.max_read_bytes {
        return Outcome::err(format!(
            "read_file {input:?} is {} bytes (max {}); use grep to slice it or pass offset/limit",
            meta.len(),
            cfg.max_read_bytes
        ));
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("read_file {input:?} failed: {e}")),
    };
    let (lines, _trailing) = split_lines(&content);
    let line_numbers = args
        .get("line_numbers")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Optional pagination: offset (1-indexed) + limit slice a window so
    // files >max_read_lines still load page-by-page instead of being refused.
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);

    // Auto-window large files when the model didn't ask for a page — dumping
    // thousands of lines into context is the #1 token waste. Explicit
    // offset/limit still honors max_read_lines.
    const AUTO_WINDOW: usize = 200;
    let auto_window = offset == 0 && limit.is_none() && lines.len() > AUTO_WINDOW;

    if offset > 0 || limit.is_some() || auto_window {
        let start = if auto_window {
            0
        } else {
            offset.saturating_sub(1).min(lines.len())
        };
        let end = if auto_window {
            AUTO_WINDOW.min(lines.len())
        } else {
            match limit {
                Some(n) => start.saturating_add(n).min(lines.len()),
                None => lines.len(),
            }
        };
        if !auto_window && end - start > cfg.max_read_lines {
            return Outcome::err(format!(
                "read_file {input:?} window is {} lines (max {}); pass a smaller limit",
                end - start,
                cfg.max_read_lines
            ));
        }
        let window = &lines[start..end];
        let mut out = String::new();
        if auto_window {
            out.push_str(&format!(
                "# {input} lines 1-{end} of {} (auto-windowed; pass offset/limit to page)\n",
                lines.len()
            ));
        } else {
            out.push_str(&format!(
                "# {input} lines {}-{} of {}\n",
                start + 1,
                end,
                lines.len()
            ));
        }
        format_read_lines(&mut out, window, start, line_numbers);
        return Outcome::ok(out);
    }
    if lines.len() > cfg.max_read_lines {
        return Outcome::err(format!(
            "read_file {input:?} has {} lines (max {}); pass offset/limit to page it",
            lines.len(),
            cfg.max_read_lines
        ));
    }
    if line_numbers {
        let mut out = String::new();
        format_read_lines(&mut out, &lines, 0, true);
        return Outcome::ok(out);
    }
    // Plain content: the model copies substrings verbatim for edit's search/replace.
    Outcome::ok(content)
}

fn format_read_lines(out: &mut String, lines: &[String], start_idx: usize, line_numbers: bool) {
    if line_numbers {
        let width = ((start_idx + lines.len()).max(1).ilog10() as usize) + 1;
        for (i, l) in lines.iter().enumerate() {
            let n = start_idx + i + 1;
            out.push_str(&format!("{n:>width$}|{l}\n"));
        }
    } else {
        for l in lines {
            out.push_str(l);
            out.push('\n');
        }
    }
}

/// Atomically write `content` to `path`: unique sibling temp, fsync, rename.
/// Uses [`crate::fsutil::atomic_write_str`] so concurrent writers (two sessions,
/// bulk+edit) never collide on a fixed `*.catalyst-code-tmp` name.
fn atomic_write_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    crate::fsutil::atomic_write_str(path, content)
}

fn write_file(input: &str, content: &str, cfg: &Config) -> Outcome {
    // Restricted paths (.env, .git/**, .ssh/**, id_rsa, …) are NO LONGER
    // hard-blocked here — enforcement moved to the approval gate
    // (main::restricted_path_for_tool) so that under Approval::Never ALL
    // restrictions are disabled, and under Destructive/Always a restricted
    // path prompts (instead of an unconditional kill) for reads AND writes.
    let path = match resolve_ws(cfg, input) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Outcome::err(format!("write_file mkdir failed: {e}"));
            }
        }
    }
    let old_content = std::fs::read_to_string(&path).unwrap_or_default();
    match atomic_write_file(&path, content) {
        Ok(_) => {
            let mut out = Outcome::ok(format!("wrote {} bytes to {input}", content.len()));
            out.diff = Some(make_unified_diff(&old_content, content, input, 3));
            out
        }
        Err(e) => Outcome::err(format!("write_file {input:?} failed: {e}")),
    }
}

fn delete_path(input: &str, cfg: &Config) -> Outcome {
    if input.is_empty() {
        return Outcome::err("delete requires a non-empty 'path'");
    }
    let path = match resolve_ws(cfg, input) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) => return Outcome::err(format!("delete {input:?} failed: {e}")),
    };
    if meta.is_dir() {
        match std::fs::remove_dir(&path) {
            Ok(()) => Outcome::ok(format!("deleted directory {input}")),
            Err(e) => Outcome::err(format!(
                "delete {input:?} failed: {e} (directories must be empty — remove contents first)"
            )),
        }
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => Outcome::ok(format!("deleted {input}")),
            Err(e) => Outcome::err(format!("delete {input:?} failed: {e}")),
        }
    }
}

fn rename_path(from: &str, to: &str, cfg: &Config) -> Outcome {
    if from.is_empty() || to.is_empty() {
        return Outcome::err("rename requires non-empty 'from' and 'to'");
    }
    let src = match resolve_ws(cfg, from) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let dst = match resolve_ws(cfg, to) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    if !src.exists() {
        return Outcome::err(format!("rename {from:?} failed: source does not exist"));
    }
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Outcome::err(format!("rename mkdir failed: {e}"));
            }
        }
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => Outcome::ok(format!("renamed {from} → {to}")),
        Err(e) => Outcome::err(format!("rename {from:?} → {to:?} failed: {e}")),
    }
}

fn mkdir_path(input: &str, cfg: &Config) -> Outcome {
    if input.is_empty() {
        return Outcome::err("mkdir requires a non-empty 'path'");
    }
    let path = match resolve_ws(cfg, input) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    match std::fs::create_dir_all(&path) {
        Ok(()) => Outcome::ok(format!("created directory {input}")),
        Err(e) => Outcome::err(format!("mkdir {input:?} failed: {e}")),
    }
}

fn list_dir(input: &str, cfg: &Config) -> Outcome {
    let path = match resolve_ws(cfg, input) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    match std::fs::read_dir(&path) {
        Ok(rd) => {
            let mut entries: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect();
            entries.sort();
            Outcome::ok(entries.join("\n"))
        }
        Err(e) => Outcome::err(format!("list_dir {input:?} failed: {e}")),
    }
}

/// Map a language/type shortcut to filename extensions.
fn type_extensions(t: &str) -> Option<&'static [&'static str]> {
    Some(match t.trim().to_ascii_lowercase().as_str() {
        "rs" | "rust" => &["rs"],
        "go" => &["go"],
        "py" | "python" => &["py", "pyi"],
        "js" | "javascript" => &["js", "jsx", "mjs", "cjs"],
        "ts" | "typescript" => &["ts", "tsx", "mts", "cts"],
        "java" => &["java"],
        "kt" | "kotlin" => &["kt", "kts"],
        "c" => &["c", "h"],
        "cpp" | "cc" | "cxx" => &["cpp", "cc", "cxx", "hpp", "hxx", "h"],
        "cs" | "csharp" => &["cs"],
        "rb" | "ruby" => &["rb"],
        "php" => &["php"],
        "swift" => &["swift"],
        "md" | "markdown" => &["md", "mdx"],
        "json" => &["json", "jsonc"],
        "yaml" | "yml" => &["yaml", "yml"],
        "toml" => &["toml"],
        "html" => &["html", "htm"],
        "css" => &["css", "scss", "sass"],
        "sh" | "bash" | "shell" => &["sh", "bash", "zsh"],
        "sql" => &["sql"],
        "xml" => &["xml"],
        "txt" | "text" => &["txt"],
        _ => return None,
    })
}

fn path_matches_type(path: &std::path::Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

/// Directory grep via `rg` when on PATH. Returns `None` to fall back to the
/// pure-Rust walker (rg missing, spawn failure, or empty non-error).
fn grep_via_rg(
    pattern: &str,
    root: &std::path::Path,
    cfg: &Config,
    case_insensitive: bool,
    globs: &[String],
    type_exts: Option<&[&str]>,
    output_mode: &str,
    head_limit: usize,
    skip: usize,
    after: usize,
    before: usize,
    invert: bool,
    fixed_string: bool,
    word: bool,
) -> Option<Outcome> {
    use std::process::{Command, Stdio};
    // Probe once: if rg isn't on PATH, skip forever for this process.
    static RG_AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let available = *RG_AVAILABLE.get_or_init(|| {
        Command::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if !available {
        return None;
    }

    let mut cmd = Command::new("rg");
    cmd.arg("--no-heading")
        .arg("--line-number")
        .arg("--color=never")
        .arg("--hidden")
        .arg("--glob")
        .arg("!.git")
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if case_insensitive {
        cmd.arg("-i");
    }
    if fixed_string {
        cmd.arg("--fixed-strings");
    }
    if word {
        cmd.arg("-w");
    }
    // rg `-l -v` lists files with ≥1 non-matching line (almost every file) —
    // rarely what a caller wants. Fall back to the pure-Rust walker so
    // files_with_matches + invert means grep -L (files with NO match).
    if invert && output_mode == "files_with_matches" {
        return None;
    }
    if invert {
        cmd.arg("-v");
    }
    match output_mode {
        "files_with_matches" => {
            cmd.arg("-l");
        }
        "count" => {
            cmd.arg("--count");
        }
        _ => {
            if after > 0 {
                cmd.arg("-A").arg(after.to_string());
            }
            if before > 0 {
                cmd.arg("-B").arg(before.to_string());
            }
        }
    }
    for g in globs {
        cmd.arg("--glob").arg(g);
    }
    if let Some(exts) = type_exts {
        for e in exts {
            cmd.arg("--glob").arg(format!("*.{e}"));
        }
    }
    // Collect more than a page so offset/head_limit still work client-side.
    let collect_cap = (skip + head_limit).saturating_mul(2).max(head_limit + skip);
    cmd.arg("--max-count")
        .arg(collect_cap.to_string())
        .arg("--")
        .arg(pattern)
        .arg(".");

    // Bound rg so a hung search can't block the tool loop forever (30s, kill child).
    use std::io::Read;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return None,
    };
    let out_h = child.stdout.take();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(mut r) = out_h {
            let _ = r.read_to_end(&mut v);
        }
        v
    });
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
    let stdout_bytes = t_out.join().unwrap_or_default();
    let status = match status {
        Some(s) => s,
        None => {
            return Some(Outcome::err(
                "grep timed out after 30s (rg killed)".to_string(),
            ));
        }
    };
    // rg exits 1 when no matches — still a successful search.
    if !status.success() && status.code() != Some(1) {
        return None;
    }
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let rel_prefix = |path: &str| -> String {
        let p = root.join(path);
        p.strip_prefix(&cfg.workspace)
            .unwrap_or(&p)
            .display()
            .to_string()
    };

    match output_mode {
        "files_with_matches" => {
            let mut files: Vec<String> = stdout
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| rel_prefix(l.trim()))
                .collect();
            files.sort();
            files.dedup();
            let total = files.len();
            let start = skip.min(total);
            let end = (start + head_limit).min(total);
            let more = end < total;
            let mut s = files[start..end].join("\n");
            if more {
                s.push_str(&format!(
                    "\n...[{} file cap reached; pass offset={} to continue]",
                    head_limit,
                    skip + (end - start)
                ));
            }
            Some(Outcome::ok(s))
        }
        "count" => {
            let mut lines: Vec<(String, usize)> = Vec::new();
            for line in stdout.lines().filter(|l| !l.is_empty()) {
                // path:count
                if let Some((path, n)) = line.rsplit_once(':') {
                    if let Ok(c) = n.parse::<usize>() {
                        lines.push((rel_prefix(path), c));
                    }
                }
            }
            let total_files = lines.len();
            let start = skip.min(total_files);
            let end = (start + head_limit).min(total_files);
            let more = end < total_files;
            let mut out_lines = Vec::new();
            let mut total = 0usize;
            for (rel, n) in &lines[start..end] {
                total += n;
                out_lines.push(format!("{rel}:{n}"));
            }
            let mut s = out_lines.join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("# total: {total}"));
            if more {
                s.push_str(&format!(
                    "\n...[{} file cap reached; pass offset={} to continue]",
                    head_limit,
                    skip + (end - start)
                ));
            }
            Some(Outcome::ok(s))
        }
        _ => {
            // content: path:line:text (or context lines with -)
            let mut records: Vec<String> = Vec::new();
            for line in stdout.lines() {
                if line.is_empty() {
                    continue;
                }
                // rg -C emits `--` between non-overlapping groups; normalize to
                // the same `...` separator the pure-Rust walker uses.
                if line == "--" {
                    records.push("...".to_string());
                    continue;
                }
                // Rewrite path prefix to workspace-relative. Match lines use
                // `path:lineno:text`; context lines use `path-lineno-text` (no
                // colon) — strip a leading `./` so both forms stay consistent.
                if let Some((path, rest)) = line.split_once(':') {
                    let path = path.strip_prefix("./").unwrap_or(path);
                    let rel = rel_prefix(path);
                    records.push(format!("{rel}:{rest}"));
                } else {
                    let line = line.strip_prefix("./").unwrap_or(line);
                    records.push(line.to_string());
                }
            }
            let total = records.len();
            let start = skip.min(total);
            let end = (start + head_limit).min(total);
            let more = end < total;
            let mut s = records[start..end].join("\n");
            if more {
                s.push_str(&format!(
                    "\n...[{} match cap reached; pass offset={} to continue]",
                    head_limit,
                    skip + (end - start)
                ));
            }
            Some(Outcome::ok(s))
        }
    }
}

#[allow(clippy::needless_range_loop)]
fn grep(pattern: &str, args: &Value, cfg: &Config) -> Outcome {
    let case_insensitive = args
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let invert = args
        .get("invert")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fixed_string = args
        .get("fixed_string")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let word = args.get("word").and_then(|v| v.as_bool()).unwrap_or(false);
    // fixed_string escapes regex metacharacters so symbols like `foo.bar()`
    // match literally; word wraps in word boundaries (\b...\b).
    let effective_pattern = {
        let base = if fixed_string {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };
        if word {
            format!(r"\b(?:{base})\b")
        } else {
            base
        }
    };
    let re = {
        let mut b = regex::RegexBuilder::new(&effective_pattern);
        b.case_insensitive(case_insensitive);
        match b.build() {
            Ok(r) => r,
            Err(e) => return Outcome::err(format!("grep bad pattern: {e}")),
        }
    };
    let input = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    // `glob` accepts a single string or an array; entries prefixed with `!`
    // are exclusions (e.g. ["**/*.rs", "!**/*test*"]). rg honors `!` natively;
    // the pure-Rust walker mirrors it via glob_filter_passes.
    let globs: Vec<String> = match args.get("glob") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    // `paths` searches a specific set of files/dirs (multi-file input). When
    // set, `path` is ignored and the rg fast-path is skipped (multiple roots).
    let paths: Vec<String> = match args.get("paths") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    let type_exts = match args
        .get("type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        Some(t) => match type_extensions(t) {
            Some(e) => Some(e),
            None => {
                return Outcome::err(format!(
                    "grep unknown type {t:?}; try rs, go, py, js, ts, java, md, json, …"
                ))
            }
        },
        None => None,
    };
    let output_mode = args
        .get("output_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("content");
    if !matches!(output_mode, "content" | "files_with_matches" | "count") {
        return Outcome::err("grep output_mode must be content, files_with_matches, or count");
    }
    let head_limit = args
        .get("head_limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 500) as usize;
    let skip = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let context = args.get("context").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let after_raw = args.get("after").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let before_raw = args.get("before").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    // Merge symmetric (context) with asymmetric (after/before) by taking the
    // max per side: context:2 == -C2; after:5 == -A5; context:2 + after:5 == -B2 -A5.
    let after = after_raw.max(context);
    let before = before_raw.max(context);

    // Resolve search roots. `paths[]` (multi-file/dir) takes precedence over
    // `path`; when set we skip the rg fast-path (it takes a single root) and
    // scan directly with the pure-Rust walker.
    let mut direct_files: Vec<std::path::PathBuf> = Vec::new();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    let single_dir_root: Option<std::path::PathBuf> = if !paths.is_empty() {
        for p in &paths {
            match resolve_ws(cfg, p) {
                Ok(r) => {
                    if r.is_file() {
                        direct_files.push(r);
                    } else {
                        dirs.push(r);
                    }
                }
                Err(e) => return Outcome::err(e),
            }
        }
        None
    } else {
        let root = if input.is_empty() {
            cfg.workspace.clone()
        } else {
            match resolve_ws(cfg, input) {
                Ok(p) => p,
                Err(e) => return Outcome::err(e),
            }
        };
        if root.is_file() {
            direct_files.push(root);
            None
        } else {
            dirs.push(root.clone());
            Some(root)
        }
    };

    // Single-directory search: prefer ripgrep (gitignore, binary skip,
    // parallelism). Fall back to the pure-Rust walker when rg is missing.
    if let Some(rd) = &single_dir_root {
        if let Some(out) = grep_via_rg(
            pattern,
            rd,
            cfg,
            case_insensitive,
            &globs,
            type_exts,
            output_mode,
            head_limit,
            skip,
            after,
            before,
            invert,
            fixed_string,
            word,
        ) {
            return out;
        }
    }

    // Records of every match: (rel_path, line_index_0based, matched_line).
    let mut records: Vec<(String, usize, String)> = Vec::new();
    let mut file_order: Vec<String> = Vec::new();
    let mut per_file: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    let mut file_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut seen = 0u32;
    let mut capped = false;
    // How many *emitted* units we've collected (matches or files, depending on mode).
    let collect_cap = skip + head_limit;

    let mut scan_file = |p: &std::path::Path| -> bool {
        // Returns true when the collect cap is hit.
        if let Some(exts) = type_exts {
            if !path_matches_type(p, exts) {
                return false;
            }
        }
        let rel = p
            .strip_prefix(&cfg.workspace)
            .unwrap_or(p)
            .display()
            .to_string();
        if !globs.is_empty() {
            let base = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !glob_filter_passes(&globs, &rel, base) {
                return false;
            }
        }
        if p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.len())
            .unwrap_or(0)
            > 40
        {
            return false;
        }
        let Ok(meta) = std::fs::metadata(p) else {
            return false;
        };
        if meta.len() > 5_000_000 {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(p) else {
            return false;
        };
        if content.contains('\0') {
            return false;
        }
        let mut n_in_file = 0usize;
        let mut any_raw_match = false;
        for (i, line) in content.lines().enumerate() {
            let raw = re.is_match(line);
            if raw {
                any_raw_match = true;
            }
            // XOR with invert: emit non-matching lines when invert is set (-v).
            if invert != raw {
                n_in_file += 1;
                if output_mode == "content" {
                    records.push((rel.clone(), i, line.to_string()));
                    let entry = per_file.entry(rel.clone()).or_default();
                    if entry.is_empty() {
                        file_order.push(rel.clone());
                    }
                    entry.push(i);
                    if records.len() >= collect_cap {
                        return true;
                    }
                }
            }
        }
        // Non-content file inclusion:
        //  - count: files with ≥1 emitted (matching, or non-matching under -v) line.
        //  - files_with_matches: normal → any match; invert → NO match (grep -L).
        if output_mode != "content" {
            let include = if output_mode == "files_with_matches" {
                invert != any_raw_match
            } else {
                n_in_file > 0
            };
            if include {
                if !file_order.iter().any(|f| f == &rel) {
                    file_order.push(rel.clone());
                }
                file_counts.insert(rel, n_in_file);
                if file_order.len() >= collect_cap {
                    return true;
                }
            }
        }
        false
    };

    for f in &direct_files {
        if scan_file(f) {
            capped = true;
            break;
        }
    }
    if !capped {
        while let Some(dir) = dirs.pop() {
            if seen > 5000 || capped {
                break;
            }
            let rd = match std::fs::read_dir(&dir) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for e in rd.flatten() {
                seen += 1;
                let p = e.path();
                let ft = match e.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ft.is_dir() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !matches!(
                        name.as_str(),
                        ".git" | "node_modules" | "target" | "dist" | "build" | ".venv"
                    ) {
                        dirs.push(p);
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                if scan_file(&p) {
                    capped = true;
                    break;
                }
            }
        }
    }

    // Apply offset + head_limit to the collected units.
    let page = |total: usize| -> (usize, usize, bool) {
        let start = skip.min(total);
        let end = (start + head_limit).min(total);
        let more = end < total || capped;
        (start, end, more)
    };

    match output_mode {
        "files_with_matches" => {
            let (start, end, more) = page(file_order.len());
            let mut s = file_order[start..end].join("\n");
            if more {
                s.push_str(&format!(
                    "\n...[{} file cap reached; pass offset={} to continue]",
                    head_limit,
                    skip + (end - start)
                ));
            }
            return Outcome::ok(s);
        }
        "count" => {
            let (start, end, more) = page(file_order.len());
            let mut lines: Vec<String> = Vec::with_capacity(end - start);
            let mut total = 0usize;
            for rel in &file_order[start..end] {
                let n = *file_counts.get(rel).unwrap_or(&0);
                total += n;
                lines.push(format!("{rel}:{n}"));
            }
            let mut s = lines.join("\n");
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&format!("# total: {total}"));
            if more {
                s.push_str(&format!(
                    "\n...[{} file cap reached; pass offset={} to continue]",
                    head_limit,
                    skip + (end - start)
                ));
            }
            return Outcome::ok(s);
        }
        _ => {} // content — fall through
    }

    // Slice records for content mode pagination.
    let (start, end, more_matches) = page(records.len());
    let records: Vec<(String, usize, String)> = records[start..end].to_vec();
    // Rebuild file_order / per_file for the page only (context mode).
    let mut page_order: Vec<String> = Vec::new();
    let mut page_per_file: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (rel, i, _) in &records {
        let entry = page_per_file.entry(rel.clone()).or_default();
        if entry.is_empty() {
            page_order.push(rel.clone());
        }
        entry.push(*i);
    }

    if after == 0 && before == 0 {
        let mut out: Vec<String> = Vec::with_capacity(records.len());
        for (rel, i, line) in &records {
            out.push(format!("{rel}:{}:{}", i + 1, line));
        }
        let mut s = out.join("\n");
        if more_matches {
            s.push_str(&format!(
                "\n...[{} match cap reached; pass offset={} to continue]",
                head_limit,
                skip + records.len()
            ));
        }
        return Outcome::ok(s);
    }

    // Context mode (like grep -C n).
    const MAX_CTX_LINES: usize = 400;
    let mut out: Vec<String> = Vec::new();
    let mut total = 0usize;
    let mut ctx_capped = false;
    for rel in &page_order {
        let path = cfg.workspace.join(rel);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        let idxs = page_per_file.get(rel).cloned().unwrap_or_default();
        let mut windows: Vec<(usize, usize)> = Vec::new();
        for &i in &idxs {
            let wstart = i.saturating_sub(before);
            let wend = (i + after).min(lines.len().saturating_sub(1));
            match windows.last_mut() {
                Some(last) if wstart <= last.1 + 1 => last.1 = last.1.max(wend),
                _ => windows.push((wstart, wend)),
            }
        }
        for (wi, (ws, we)) in windows.iter().enumerate() {
            if wi > 0 {
                out.push("...".to_string());
            }
            for ln in *ws..=*we {
                if total >= MAX_CTX_LINES {
                    ctx_capped = true;
                    break;
                }
                let matched = idxs.binary_search(&ln).is_ok();
                let sep = if matched { ':' } else { '-' };
                out.push(format!("{rel}{sep}{}{sep}{}", ln + 1, lines[ln]));
                total += 1;
            }
            if ctx_capped {
                break;
            }
        }
        if ctx_capped {
            break;
        }
    }
    let mut s = out.join("\n");
    if more_matches || ctx_capped {
        s.push_str(&format!(
            "\n...[output cap reached; pass offset={} to continue]",
            skip + records.len()
        ));
    }
    Outcome::ok(s)
}

fn glob(pattern: &str, cfg: &Config) -> Outcome {
    let mut out: Vec<String> = Vec::new();
    walk_glob(&cfg.workspace, &cfg.workspace, pattern, &mut out, 0);
    if out.is_empty() {
        Outcome::ok("")
    } else if out.len() > 200 {
        out.truncate(200);
        Outcome::ok(out.join("\n") + "\n...[200 result cap reached]")
    } else {
        Outcome::ok(out.join("\n"))
    }
}

// ponytail: hand-rolled glob. Supports *, **, ?, and literal segments.
// Not a full POSIX glob; covers the common **/*.ext and dir/*.rs patterns.
fn walk_glob(
    root: &std::path::Path,
    dir: &std::path::Path,
    pattern: &str,
    out: &mut Vec<String>,
    depth: usize,
) {
    if out.len() >= 200 || depth > 15 {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        if out.len() >= 200 {
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "node_modules" | "target" | "dist" | "build" | ".venv"
        ) {
            continue;
        }
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if glob_match(pattern, &rel) {
            out.push(rel.clone());
        }
        if is_dir {
            walk_glob(root, &p, pattern, out, depth + 1);
        }
    }
}

/// Expand bash-style `{a,b,c}` alternatives in a glob (including nested
/// braces). Cursor/Claude models routinely emit `**/*.{rs,go,md}`; without
/// expansion those patterns match literally and grep/glob return empty.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let mut start = None;
    let mut depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let s = start.expect("brace depth paired");
                    let inner = &pattern[s + 1..i];
                    // Only treat as alternation when the top-level group has a comma.
                    if brace_group_has_comma(inner) {
                        let prefix = &pattern[..s];
                        let suffix = &pattern[i + 1..];
                        let mut out = Vec::new();
                        for alt in split_brace_alts(inner) {
                            let combined = format!("{prefix}{alt}{suffix}");
                            out.extend(expand_braces(&combined));
                        }
                        return out;
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    vec![pattern.to_string()]
}

fn brace_group_has_comma(inner: &str) -> bool {
    let mut depth = 0usize;
    for b in inner.bytes() {
        match b {
            b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            b',' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn split_brace_alts(inner: &str) -> Vec<&str> {
    let mut alts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, b) in inner.bytes().enumerate() {
        match b {
            b'{' => depth += 1,
            b'}' if depth > 0 => depth -= 1,
            b',' if depth == 0 => {
                alts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    alts.push(&inner[start..]);
    alts
}

fn glob_match(pattern: &str, name: &str) -> bool {
    expand_braces(pattern)
        .into_iter()
        .any(|p| glob_match_one(&p, name))
}

/// Apply a set of include/exclude globs (rg `--glob` semantics). A file passes
/// when it matches at least one inclusion (or there are no inclusions) AND does
/// not match any exclusion. Entries prefixed with `!` are exclusions. Each glob
/// is tested against both the workspace-relative path and the bare basename.
fn glob_filter_passes(globs: &[String], rel: &str, base: &str) -> bool {
    let mut includes: Vec<&str> = Vec::new();
    let mut excludes: Vec<&str> = Vec::new();
    for g in globs {
        match g.strip_prefix('!') {
            Some(ex) => excludes.push(ex),
            None => includes.push(g.as_str()),
        }
    }
    let any_match = |pats: &[&str]| {
        pats.iter()
            .any(|p| glob_match(p, rel) || glob_match(p, base))
    };
    let inc_ok = includes.is_empty() || any_match(&includes);
    let exc_ok = !any_match(&excludes);
    inc_ok && exc_ok
}

fn glob_match_one(pattern: &str, name: &str) -> bool {
    // ponytail: convert glob to a simple matcher. ** matches any path depth.
    // Handle the common cases; fall back to substring match.
    if pattern.contains("**") {
        let suffix = pattern.replace("**/", "").replace("**", "");
        if suffix.is_empty() {
            return true;
        }
        // Match the suffix (which may contain * and ?) against the file's basename, or as a path suffix for multi-segment suffixes.
        if suffix.contains('/') {
            return name == suffix || name.ends_with(&format!("/{suffix}"));
        }
        let basename = name.rsplit('/').next().unwrap_or(name);
        return star_match(&suffix, basename);
    }
    // single-segment glob with * and ?
    if !pattern.contains('/') {
        return star_match(pattern, name);
    }
    // multi-segment: match segment by segment
    let ps: Vec<&str> = pattern.split('/').collect();
    let ns: Vec<&str> = name.split('/').collect();
    if ps.len() != ns.len() {
        return false;
    }
    ps.iter().zip(ns.iter()).all(|(p, n)| star_match(p, n))
}

fn star_match(pat: &str, s: &str) -> bool {
    // ponytail: classic * and ? glob without a crate.
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    glob_dp(&p, &t)
}

fn glob_dp(p: &[char], t: &[char]) -> bool {
    // DP matching for * (any run) and ? (one char).
    let mut dp = vec![vec![false; t.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=t.len() {
            match p[i - 1] {
                '*' => dp[i][j] = dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i][j] = dp[i - 1][j - 1],
                c => dp[i][j] = dp[i - 1][j - 1] && c == t[j - 1],
            }
        }
    }
    dp[p.len()][t.len()]
}

// ---- bash (async, timeout + kill) ----

/// Collapse runs of whitespace to a single space and trim, so `rm  -rf  /`
/// can't evade a `rm -rf /` denylist pattern (P1-7).
fn normalize_bash_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Find `needle` in `haystack` only where the character right after the match
/// is end-of-string or whitespace. Used for path-targeting denylist patterns
/// (e.g. `rm -rf /`) so they match `rm -rf /` and `rm -rf / ` but NOT
/// `rm -rf /tmp/x` — i.e. the `/` must be the end of a path token, not the
/// prefix of `/tmp` (P1-7: removes the false-positive that blocked legit
/// `rm -rf /tmp/...` cleanup).
fn contains_at_boundary(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let after_is_boundary = end >= bytes.len() || bytes[end] == b' ';
        if after_is_boundary {
            return true;
        }
        from = end.max(start + 1);
    }
    false
}

/// Truncate `output` to `cap` bytes, keeping the tail (where errors usually
/// live) and salvaging error/warning lines from the dropped head. A pure
/// tail truncation loses a compile error that sits in the *middle* of a huge
/// build log; this keeps the tail plus the first few matching head lines so
/// the model still sees the root cause. UTF-8 safe: the tail slice is walked
/// back to a char boundary.
pub(crate) fn smart_truncate(output: &str, cap: usize) -> String {
    if output.len() <= cap {
        return output.to_string();
    }
    // Roughly 60% tail, 40% for salvaged head lines. The tail is the part that
    // almost always matters; head salvage is best-effort.
    let tail_budget = cap * 3 / 4;
    let head_budget = cap.saturating_sub(tail_budget);

    // Salvage error/warning lines from the head (the bytes we're dropping).
    let split = output.len() - tail_budget;
    let split = {
        let mut s = split;
        while !output.is_char_boundary(s) {
            s += 1;
        }
        s
    };
    let head = &output[..split];
    let tail = &output[split..];

    let errorish = regex::Regex::new(
        r"(?i)^(?:error|warning|error\[|error:|warning:|note:|help:|\s*--\>\s|panic|fatal|failed|undefined|cannot|exception|not found|no such|denied|traceback)",
    )
    .expect("static salvage regex");
    let mut salvaged: Vec<&str> = Vec::new();
    let mut salvaged_bytes = 0usize;
    for line in head.lines().rev() {
        if !errorish.is_match(line) {
            continue;
        }
        let b = line.len() + 1; // +newline
        if salvaged_bytes + b > head_budget {
            break;
        }
        salvaged_bytes += b;
        salvaged.push(line);
    }
    salvaged.reverse(); // back to file order

    let mut out = String::with_capacity(cap + 128);
    if salvaged.is_empty() {
        out.push_str(&format!(
            "...[output truncated, showing last {}KB]...\n",
            tail_budget / 1024
        ));
    } else {
        out.push_str(&format!(
            "...[output truncated: {} salvaged error/warning line(s) from the head + last {}KB]...\n",
            salvaged.len(),
            tail_budget / 1024
        ));
        for l in &salvaged {
            out.push_str(l);
            out.push('\n');
        }
        out.push_str("--- tail ---\n");
    }
    out.push_str(tail);
    out
}

/// Detect whether a bash command invokes `sudo`. Matches `sudo` as a
/// standalone word (word-boundary) anywhere in the command. Over-matches on
/// strings like `echo sudo` (false positive) — that's acceptable: better to
/// prompt for approval than to let sudo grab /dev/tty and garble the TUI.
pub fn command_uses_sudo(command: &str) -> bool {
    // sudo handling (the `sudo()` wrapper + the password flyout) is a POSIX
    // concern — Windows has no `sudo`/`/dev/tty` machinery to reroute. On
    // PowerShell this returns false so the whole sudo prompt path is skipped
    // and the command runs through the normal approval gate instead.
    if !shell_is_posix() {
        return false;
    }
    static SUDO_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\bsudo\b").expect("sudo regex"));
    SUDO_RE.is_match(command)
}

/// Result of checking whether sudo can authenticate without user input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SudoPreflight {
    /// Sudo is ready (NOPASSWD, a valid credential timestamp, or root).
    NonInteractive,
    /// Sudo explicitly reported that authentication requires a password.
    PasswordRequired,
    /// Sudo could not be checked or failed for a reason a password cannot fix.
    Unavailable,
}

/// Classify a `sudo -n true` result.  Keep this deliberately narrow: in
/// permissive mode an absent sudo binary, a sudoers denial, or a broken policy
/// must produce a normal command failure, not a misleading password prompt.
fn classify_sudo_preflight(success: bool, stderr: &[u8]) -> SudoPreflight {
    if success {
        return SudoPreflight::NonInteractive;
    }

    // The probe sets LC_ALL=C so these are stable sudo diagnostics.  The
    // additional variants cover older sudo/PAM combinations.
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if stderr.contains("password is required")
        || stderr.contains("password required")
        || stderr.contains("no tty present and no askpass program specified")
        || stderr.contains("a terminal is required to read the password")
    {
        SudoPreflight::PasswordRequired
    } else {
        SudoPreflight::Unavailable
    }
}

/// Check whether sudo can run without a password. The probe is always
/// non-interactive and can never open `/dev/tty`.
pub async fn sudo_preflight(cfg: &Config) -> SudoPreflight {
    // POSIX-only (never reached on Windows: command_uses_sudo is false there,
    // so the caller's `if tools::command_uses_sudo(cmd)` branch is skipped).
    if !shell_is_posix() {
        return SudoPreflight::Unavailable;
    }
    let mut cmd = tokio::process::Command::new("sudo");
    cmd.args(["-n", "true"]);
    cmd.current_dir(&cfg.workspace);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    cmd.env(
        "PATH",
        std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
    );
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");

    // A timeout/spawn error is not evidence that the account has a password.
    // In Never mode it therefore falls through to a non-interactive execution,
    // which reports the real error without opening a UI prompt.
    match tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(output)) => classify_sudo_preflight(output.status.success(), &output.stderr),
        _ => SudoPreflight::Unavailable,
    }
}

/// Sudo prompts are policy prompts outside permissive (`Never`) mode. In
/// permissive mode they are authentication-only and appear solely when sudo
/// explicitly says a password is required.
pub fn sudo_should_prompt(approval: &Approval, preflight: SudoPreflight) -> bool {
    !matches!(approval, Approval::Never) || matches!(preflight, SudoPreflight::PasswordRequired)
}

/// How to handle sudo when the command invokes it.
pub enum SudoAuth {
    /// No sudo auth available. If the command uses sudo, returns a clean error
    /// so the caller can surface a prompt. Used by subagents / bulk.
    None,
    /// Run with `sudo -S` and feed this password on stdin (user approved).
    Password(String),
    /// Run with `sudo -n` (non-interactive). Succeeds if NOPASSWD or cached
    /// credentials exist; fails cleanly (never opens /dev/tty) if a password
    /// is needed or sudo is unavailable. Used whenever approval is Never and
    /// the preflight did not explicitly identify a password requirement.
    NonInteractive,
}

/// Run bash with cwd=workspace, a real timeout, and a denylist tripwire.
/// Optional hard sandbox: --sandbox firejail wraps the command in a
/// firejail profile that whitelists only the workspace; --no-network adds
/// `unshare -n` so the command can't phone home. Both are belt-and-suspenders
/// on top of the denylist tripwire.
///
/// `sudo_password`: when Some, the command is known to invoke `sudo` and the
/// user approved it + supplied a password. The password is fed on stdin and
/// `sudo` is forced to read it (`-S`) instead of opening /dev/tty (which would
/// garble the TUI). When None but the command uses sudo, a clean error is
/// returned so the caller can surface an approval prompt instead.
pub async fn execute_bash(
    command: &str,
    cfg: &Config,
    timeout_override: Option<u64>,
    sudo_auth: SudoAuth,
) -> Outcome {
    // ponytail: denylist is a tripwire, not a sandbox. It blocks the most
    // catastrophic obvious commands; a determined model bypasses it.
    // Normalize whitespace first so `rm  -rf  /` (extra spaces) can't slip past
    // a `rm -rf /` pattern (P1-7). Path-targeting patterns (ending in `/` or
    // `~`) are matched at a token boundary so `rm -rf /` doesn't false-positive
    // on `rm -rf /tmp/x` (the `/` must be the end of a path token).
    let norm = normalize_bash_ws(command);
    let lower = norm.to_ascii_lowercase();
    for bad in &cfg.bash_deny {
        let bad_l = bad.to_ascii_lowercase();
        let blocked = if bad_l.ends_with('/') || bad_l.ends_with('~') {
            contains_at_boundary(&lower, &bad_l)
        } else {
            lower.contains(&bad_l)
        };
        if blocked {
            return Outcome::err(format!("bash command blocked by denylist (matched '{bad}'); use a sandbox for hard isolation"));
        }
    }
    // Regex denylist: match against whitespace-normalized command (same as
    // string denylist) so `curl  http://evil` can't bypass `curl\s+http`.
    for re in &cfg.bash_deny_regex_compiled {
        if re.is_match(&norm) {
            return Outcome::err(format!("bash command blocked by regex denylist (matched '{}'); use a sandbox for hard isolation", re.as_str()));
        }
    }

    // Sudo handling: sudo by default reads the password from /dev/tty (the
    // controlling terminal), which garbles the TUI's rendering. We never let
    // sudo reach /dev/tty. The `sudo() { command sudo -S "$@"; }` redefinition
    // is POSIX-shell syntax and only applies on the HOST (a microVM guest has
    // no host sudo). Inside the sandbox, sudo is rejected outright.
    let uses_sudo = command_uses_sudo(command);
    let sandboxed = crate::sandbox::is_sandbox_enabled();
    let run_command = if sandboxed {
        if uses_sudo {
            return Outcome::err(
                "sudo is not supported inside the sandbox microVM. The guest runs as a\
                 \nconfined user without host privileges; run the command without sudo, or\
                 \ndisable sandboxing with `--sandbox none`.",
            );
        }
        command.to_string()
    } else {
        match &sudo_auth {
            SudoAuth::Password(_) if uses_sudo && shell_is_posix() => {
                format!(r#"sudo() {{ command sudo -S "$@"; }}; {command}"#)
            }
            SudoAuth::NonInteractive if uses_sudo && shell_is_posix() => {
                format!(r#"sudo() {{ command sudo -n "$@"; }}; {command}"#)
            }
            _ => {
                if uses_sudo {
                    return Outcome::err(
                        "this command uses sudo, which requires interactive approval. \
                         The user must approve it in the main session — ask them to run it \
                         manually, or re-run without sudo.",
                    );
                }
                command.to_string()
            }
        }
    };

    // Build the argv. The active shell decides the form: POSIX `bash -c <cmd>`
    // (the guest is always Linux `bash` when sandboxed) or PowerShell on a
    // Windows host. The execution backend decides WHERE it runs (host or
    // microVM); tools never spawn tokio::process::Command directly anymore.
    let (program, args) = crate::sandbox::policy::shell_argv(&run_command);

    // Per-call timeout override (the bash tool's `timeout` arg): clamp to
    // [1, max_bash_timeout_secs] so a model can buy more time for a slow
    // build/test but can't escalate past the configured ceiling.
    let secs = match timeout_override {
        Some(t) => t.clamp(1, cfg.max_bash_timeout_secs.max(1)),
        None => cfg.bash_timeout_secs,
    };
    let timeout = std::time::Duration::from_secs(secs);

    // Stdin: the sudo password (host path) when feeding sudo; otherwise null.
    let feeding_password = !sandboxed && uses_sudo && matches!(&sudo_auth, SudoAuth::Password(_));
    let stdin = if feeding_password {
        if let SudoAuth::Password(pw) = &sudo_auth {
            Some(format!("{pw}\n").into_bytes())
        } else {
            None
        }
    } else {
        None
    };

    let proc_env =
        crate::sandbox::policy::build_process_env(cfg, crate::sandbox::policy::ExecPurpose::Bash);
    let cwd =
        crate::sandbox::policy::effective_cwd(cfg, "").unwrap_or_else(|_| cfg.workspace.clone());
    let req = crate::sandbox::ExecRequest {
        program,
        args,
        cwd,
        env: proc_env.env,
        inherit_parent_env: proc_env.inherit_parent,
        stdin,
        timeout,
        ..Default::default()
    };

    match crate::sandbox::execution_backend().execute(req).await {
        Ok(r) => {
            let mut combined = String::new();
            if !r.stdout.is_empty() {
                combined.push_str(&String::from_utf8_lossy(&r.stdout));
            }
            if !r.stderr.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n--- stderr ---\n");
                }
                combined.push_str(&String::from_utf8_lossy(&r.stderr));
            }
            if combined.is_empty() {
                combined.push_str("(no output)");
            }
            const CAP: usize = 32_768;
            if combined.len() > CAP {
                combined = smart_truncate(&combined, CAP);
            }
            if r.timed_out {
                Outcome {
                    ok: false,
                    output: format!("bash timed out after {secs}s (killed)\n{combined}"),
                    diff: None,
                }
            } else {
                let ok = r.exit_code == Some(0);
                // Feed recurring technical failures (compile/test/panic signatures)
                // into the failure atlas so future error-recovery can surface
                // "you've hit this N times before". Gated on diagnostic-looking
                // output so a typo or `grep` exit-1 doesn't flood the atlas.
                if !ok && crate::failure_atlas::looks_like_diagnostic_output(&combined) {
                    let pid = crate::project_identity::resolve_project_identity(&cfg.workspace).id;
                    crate::failure_atlas::record_diagnostic(&pid, "bash", &combined);
                }
                Outcome {
                    ok,
                    output: combined,
                    diff: None,
                }
            }
        }
        Err(e) => Outcome::err(e.user_message()),
    }
}

/// Resolve the shell program used to run `bash`-tool commands.
///
/// Single source of truth: [`crate::sandbox::policy::resolve_host_shell`].
#[allow(dead_code)] // kept as the tools-layer alias; policy is the authority
pub(crate) fn resolve_shell() -> String {
    crate::sandbox::policy::resolve_host_shell()
}

/// Whether the resolved shell is a POSIX shell (`bash`/`sh`/`zsh`/`dash`/…):
/// it takes `-c <cmd>` and supports the `sudo()` function-wrapper trick.
/// False for PowerShell (`powershell`/`pwsh`) and cmd.exe. Keyed on the
/// resolved shell (not the host OS) so a WSL `bash` on Windows still behaves
/// as bash and a `pwsh` override on Linux behaves as PowerShell.
pub(crate) fn shell_is_posix() -> bool {
    crate::sandbox::policy::effective_shell_kind().is_posix()
}

/// Build `(program, args)` for running a single command string in the active
/// shell. Delegates to [`crate::sandbox::policy::shell_argv`] so diagnostics
/// and the bash tool share cmd/PowerShell/POSIX argv forms.
pub(crate) fn shell_argv(command: &str) -> (String, Vec<String>) {
    crate::sandbox::policy::shell_argv(command)
}

/// Is `prog` on PATH? A minimal `which` used to prefer `pwsh` over
/// `powershell` on Windows without a hard dependency. Cached at first use
/// by `pwsh_available`. POSIX hosts never need this — they default to `bash`.
#[cfg(target_os = "windows")]
fn which(prog: &str) -> bool {
    let path = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(format!("{prog}.exe"));
        if candidate.is_file() {
            return true;
        }
    }
    false
}

#[cfg(target_os = "windows")]
pub(crate) fn pwsh_available() -> bool {
    static CACHED: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| which("pwsh"));
    *CACHED
}

// ---- search/replace edit ----

fn split_lines(content: &str) -> (Vec<String>, bool) {
    let trailing_nl = content.ends_with('\n');
    if content.is_empty() {
        return (Vec::new(), false);
    }
    let mut v: Vec<String> = content.split('\n').map(String::from).collect();
    if trailing_nl {
        v.pop();
    }
    (v, trailing_nl)
}

/// Collapse every run of whitespace to a single space (and trim ends), returning
/// the normalized string plus a map from each normalized char's index to the
/// byte offset where its *first* source char begins. Used for
/// whitespace-tolerant edit matching: a search with drifted indentation still
/// locates the right region, and the map projects the match back onto the
/// original bytes so the replacement edits the real text, not the normalized
/// copy.
fn normalize_ws_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    let mut prev_ws = false;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if !prev_ws && !out.is_empty() {
                out.push(' ');
                map.push(i);
            }
            prev_ws = true;
        } else {
            out.push(c);
            map.push(i);
            prev_ws = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
        map.pop();
    }
    (out, map)
}

/// Find every non-overlapping occurrence of `search` in `content`. With
/// `normalize` true, matching runs on whitespace-collapsed forms and the
/// returned spans are byte ranges in the *original* content (so a drifted
/// match still edits the real text). Without `normalize`, spans are the exact
/// substring byte ranges.
fn find_matches(content: &str, search: &str, normalize: bool) -> Vec<(usize, usize)> {
    if search.is_empty() {
        return Vec::new();
    }
    if !normalize {
        let mut out = Vec::new();
        let mut from = 0usize;
        while let Some(pos) = content[from..].find(search) {
            let s = from + pos;
            out.push((s, s + search.len()));
            from = s + search.len();
        }
        return out;
    }
    let (nsearch, _) = normalize_ws_with_map(search);
    if nsearch.is_empty() {
        return Vec::new();
    }
    let (ncontent, map) = normalize_ws_with_map(content);
    // `map` has ONE entry per kept CHAR of the normalized string (`map[k]` =
    // the byte offset of the k-th char). But `str::find` returns a BYTE offset.
    // For ASCII these coincide; for any multi-byte content (CJK, emoji, smart
    // quotes, `→`/`…`/`—`) indexing `map` with a byte offset either panics (OOB)
    // or returns the wrong span — silently corrupting the file via
    // replace_range. Track the byte offset (`from`/`p`, for str slicing) AND
    // the char index (`from_char`/`p_char`, for map indexing) in parallel.
    let nlen = nsearch.len();
    let nlen_chars = nsearch.chars().count();
    let mut out = Vec::new();
    let mut from = 0usize; // byte offset in ncontent
    let mut from_char = 0usize; // char index in ncontent
    while let Some(pos) = ncontent[from..].find(&nsearch) {
        let p = from + pos; // byte offset of the match start
                            // char index of the match start = from_char + chars in the gap
        let p_char = from_char + ncontent[from..p].chars().count();
        let start_orig = map[p_char];
        let end_norm_char = p_char + nlen_chars;
        let end_orig = if end_norm_char < map.len() {
            // Start of the next kept char sits right after any whitespace that
            // was collapsed between the last matched char and it — so this
            // includes the matched region's internal whitespace (correct) and
            // excludes trailing gap whitespace (also correct).
            map[end_norm_char]
        } else {
            // Match runs to the end of the normalized content: end right after
            // the last matched SOURCE char, not content.len(), so trailing
            // whitespace the normalizer trimmed (e.g. a final newline) isn't
            // consumed by the replacement.
            let last_start = map[p_char + nlen_chars - 1];
            last_start
                + content[last_start..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0)
        };
        out.push((start_orig, end_orig));
        from = p + nlen;
        from_char = p_char + nlen_chars;
    }
    out
}

/// A best-effort hint for a failed search: the content line sharing the most
/// whitespace tokens with the search, with its 1-indexed line number. Lets the
/// model self-correct in one shot instead of re-reading the whole file when the
/// only drift is a typo or a nearby line.
fn closest_hint(content: &str, search: &str) -> String {
    let search_tokens: Vec<&str> = search.split_whitespace().collect();
    if search_tokens.is_empty() {
        return String::new();
    }
    let mut best: Option<(usize, usize, &str)> = None; // (overlap, lineno, line)
    for (idx, line) in content.lines().enumerate() {
        let line_tokens: std::collections::HashSet<&str> = line.split_whitespace().collect();
        let overlap = search_tokens
            .iter()
            .filter(|t| line_tokens.contains(*t))
            .count();
        if overlap == 0 {
            continue;
        }
        if best.is_none() || best.is_some_and(|(o, _, _)| overlap > o) {
            best = Some((overlap, idx + 1, line));
        }
    }
    match best {
        Some((o, lineno, line)) => {
            let snip: String = line.chars().take(120).collect();
            format!("closest match: line {lineno} ({o} token(s) in common): {snip}")
        }
        None => String::new(),
    }
}

/// Resolve, read, and apply a list of search/replace edits in memory — WITHOUT
/// writing. Returns (path, old_content, new_content) so both the writing path
/// (`execute_edit`) and the approval-preview path (`preview_diff_edit`) share
/// one source of truth. Each edit may set `replace_all` (replace every match,
/// not just a unique one) and `normalize_whitespace` (match on whitespace-
/// collapsed text so indentation/spacing drift still lands). On a not-found or
/// ambiguous search the file is left untouched and an error is returned.
fn plan_edit(
    input: &str,
    edits: &[Value],
    cfg: &Config,
) -> Result<(std::path::PathBuf, String, String), String> {
    let path = resolve_ws(cfg, input)?;
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("edit: read {input:?} failed: {e}"))?;

    // Fail-closed pre-scan: detect the replace_all-clobbers-insertion conflict
    // BEFORE applying anything. When a later edit uses replace_all with search S
    // and an EARLIER edit's replace text contains S, the replace_all would run
    // on the evolved content and silently rewrite the just-inserted text. This
    // caused silent corruption (e.g. a helper inserted then rewritten into
    // self-recursion). Reject the whole batch with a clear error instead — the
    // caller should split into separate edit calls.
    let parsed: Vec<(usize, &str, &str, bool, bool)> = edits
        .iter()
        .enumerate()
        .map(|(i, ev)| {
            (
                i,
                ev.get("search").and_then(|v| v.as_str()).unwrap_or(""),
                ev.get("replace").and_then(|v| v.as_str()).unwrap_or(""),
                ev.get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                ev.get("normalize_whitespace")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            )
        })
        .collect();
    for (i, search_i, _replace_i, replace_all_i, _norm_i) in &parsed {
        if *replace_all_i && !search_i.is_empty() {
            for (j, _search_j, replace_j, _ra_j, _n_j) in &parsed {
                if *j < *i && replace_j.contains(search_i) {
                    return Err(format!(
                        "edit #{i}: replace_all search `{search_i}` also appears in edit #{j}'s replacement text. \
                         A replace_all runs on the evolved content and would silently rewrite the text edit #{j} just inserted. \
                         Split this into separate edit calls: apply the replace_all FIRST, then the insertion (or make the inserted text not contain `{search_i}`)."
                    ));
                }
            }
        }
    }

    let mut new_content = content.clone();

    for (i, ev) in edits.iter().enumerate() {
        let search = ev.get("search").and_then(|v| v.as_str()).unwrap_or("");
        let replace = ev.get("replace").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = ev
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let normalize = ev
            .get("normalize_whitespace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if search.is_empty() {
            return Err(format!(
                "edit #{i}: 'search' must not be empty (use write_file for new files)"
            ));
        }
        let spans = find_matches(&new_content, search, normalize);
        if spans.is_empty() {
            let hint = closest_hint(&new_content, search);
            let hint_part = if hint.is_empty() {
                String::new()
            } else {
                format!("\n{hint}")
            };
            return Err(format!(
                "edit #{i}: search text not found in {input:?}; re-read the file and copy the exact text (watch whitespace). Search was:\n{}{hint_part}",
                search
            ));
        }
        if spans.len() > 1 && !replace_all {
            return Err(format!(
                "edit #{i}: search text matches {} places in {input:?}; include more surrounding lines so the match is unique, or set replace_all:true to replace all of them. Search was:\n{}",
                spans.len(),
                search
            ));
        }
        if replace_all {
            // Replace right-to-left so earlier spans' byte offsets stay valid.
            let mut spans = spans;
            spans.sort_by_key(|s| std::cmp::Reverse(s.0));
            for (s, e) in spans {
                new_content.replace_range(s..e, replace);
            }
        } else {
            let (s, e) = spans[0];
            new_content.replace_range(s..e, replace);
        }
    }
    Ok((path, content, new_content))
}

/// Apply a list of search/replace edits to a file atomically. Each `search`
/// string must match the current file content exactly and uniquely; edits apply
/// in order to the evolving content. If any search is not found or is
/// ambiguous, the file is left untouched and an error is returned.
fn execute_edit(input: &str, edits: &[Value], cfg: &Config) -> Outcome {
    let (path, old_content, new_content) = match plan_edit(input, edits, cfg) {
        Ok(v) => v,
        Err(e) => return Outcome::err(e),
    };
    if let Err(e) = atomic_write_file(&path, &new_content) {
        return Outcome::err(format!("edit: write {input:?} failed: {e}"));
    }
    let mut out = Outcome::ok(format!("applied {} edit(s)", edits.len()));
    out.diff = Some(make_unified_diff(&old_content, &new_content, input, 3));
    out
}

/// Compute the unified diff an `edit` call *would* produce, without writing.
/// Used by the approval gate so the human sees the resulting change before
/// approving, not just the raw search/replace blobs. Returns Ok(diff) (possibly
/// empty if identical) or Err(reason) if the edit wouldn't apply.
pub fn preview_diff_edit(input: &str, edits: &[Value], cfg: &Config) -> Result<String, String> {
    let (_path, old_content, new_content) = plan_edit(input, edits, cfg)?;
    Ok(make_unified_diff(&old_content, &new_content, input, 3))
}

/// Compute the unified diff a `patch` call *would* produce, without writing.
pub fn preview_diff_patch(path: &str, patch: &str, cfg: &Config) -> Result<String, String> {
    let resolved = resolve_ws(cfg, path)?;
    let original = std::fs::read_to_string(&resolved).unwrap_or_default();
    let new = apply_unified_diff(&original, patch)?;
    Ok(make_unified_diff(&original, &new, path, 3))
}

/// Compute the unified diff a `write_file` call *would* produce, without
/// writing. For a new file the diff is the whole content as additions.
pub fn preview_diff_write(input: &str, content: &str, cfg: &Config) -> Result<String, String> {
    let path = resolve_ws(cfg, input)?;
    let old_content = std::fs::read_to_string(&path).unwrap_or_default();
    Ok(make_unified_diff(&old_content, content, input, 3))
}

// ---- bulk tools ----
// ponytail: thin batch wrappers over the single-file primitives. Each entry
// gets its own result block so per-file errors don't abort the whole batch.

/// Read many files. Each file becomes a headed block; per-file errors inline.
/// Total output is capped so a large batch cannot dump tens of thousands of
/// tokens in one result — callers should page with fewer paths or use grep.
fn bulk_read(args: &Value, cfg: &Config) -> Outcome {
    let Some(paths) = args.get("paths").and_then(|v| v.as_array()) else {
        return Outcome::err("bulk_read requires a 'paths' array");
    };
    if paths.is_empty() {
        return Outcome::err("bulk_read requires a non-empty 'paths' array");
    }
    const MAX_PATHS: usize = 20;
    const MAX_TOTAL_BYTES: usize = 48 * 1024;
    if paths.len() > MAX_PATHS {
        return Outcome::err(format!(
            "bulk_read accepts at most {MAX_PATHS} paths (got {}); split the batch or use grep",
            paths.len()
        ));
    }
    let mut blocks: Vec<String> = Vec::with_capacity(paths.len());
    let mut ok = true;
    let mut total = 0usize;
    for (i, p) in paths.iter().enumerate() {
        if total >= MAX_TOTAL_BYTES {
            ok = false;
            blocks.push(format!(
                "### [{i}..] <budget exhausted>\nerror: bulk_read total output capped at {MAX_TOTAL_BYTES} bytes; request fewer paths or page with read_file offset/limit"
            ));
            break;
        }
        let Some(path) = p.as_str() else {
            ok = false;
            blocks.push(format!(
                "### [{i}] <invalid path>\nerror: path must be a string"
            ));
            continue;
        };
        let r = read_file(path, &json!({ "path": path }), cfg);
        if !r.ok {
            ok = false;
        }
        let mut block = format!("### [{i}] {path}\n{}", r.output);
        if total + block.len() > MAX_TOTAL_BYTES {
            let room = MAX_TOTAL_BYTES.saturating_sub(total);
            block = smart_truncate(&block, room.max(256));
            blocks.push(block);
            ok = false;
            blocks.push(format!(
                "### [remaining] <budget exhausted>\nerror: bulk_read total output capped at {MAX_TOTAL_BYTES} bytes"
            ));
            break;
        }
        total += block.len();
        blocks.push(block);
    }
    Outcome {
        ok,
        output: blocks.join("\n\n"),
        diff: None,
    }
}

/// Write many files. One status line per file; ok only if every write succeeded.
fn bulk_write(args: &Value, cfg: &Config) -> Outcome {
    let Some(files) = args.get("files").and_then(|v| v.as_array()) else {
        return Outcome::err("bulk_write requires a 'files' array");
    };
    if files.is_empty() {
        return Outcome::err("bulk_write requires a non-empty 'files' array");
    }
    let mut lines: Vec<String> = Vec::with_capacity(files.len());
    let mut ok = true;
    for (i, f) in files.iter().enumerate() {
        let path = f.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            ok = false;
            lines.push(format!("[{i}] error: missing 'path'"));
            continue;
        }
        let content = match require_string_arg(f, "content") {
            Ok(c) => c,
            Err(e) => {
                ok = false;
                lines.push(format!("[{i}] {path}: error: {e}"));
                continue;
            }
        };
        let r = write_file(path, content, cfg);
        if !r.ok {
            ok = false;
        }
        lines.push(format!("[{i}] {path}: {}", r.output));
    }
    Outcome {
        ok,
        output: lines.join("\n"),
        diff: None,
    }
}

/// Apply edits to many files. Each file's edits apply atomically to one snapshot;
/// a failed search (not found / not unique) fails only that file's block.
fn bulk_edit(args: &Value, cfg: &Config) -> Outcome {
    let Some(edits) = args.get("edits").and_then(|v| v.as_array()) else {
        return Outcome::err("bulk_edit requires an 'edits' array");
    };
    if edits.is_empty() {
        return Outcome::err("bulk_edit requires a non-empty 'edits' array");
    }
    let mut blocks: Vec<String> = Vec::with_capacity(edits.len());
    let mut ok = true;
    for (i, e) in edits.iter().enumerate() {
        let path = e.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            ok = false;
            blocks.push(format!("### [{i}] <missing path>\nerror: missing 'path'"));
            continue;
        }
        let Some(file_edits) = e.get("edits").and_then(|v| v.as_array()) else {
            ok = false;
            blocks.push(format!("### [{i}] {path}\nerror: missing 'edits' array"));
            continue;
        };
        if file_edits.is_empty() {
            ok = false;
            blocks.push(format!("### [{i}] {path}\nerror: empty 'edits' array"));
            continue;
        }
        // Wrap as an edit tool call and reuse execute_edit.
        let wrapped = json!({ "path": path, "edits": file_edits });
        let r = execute("edit", &wrapped, cfg);
        if !r.ok {
            ok = false;
        }
        blocks.push(format!("### [{i}] {path}\n{}", r.output));
    }
    Outcome {
        ok,
        output: blocks.join("\n\n"),
        diff: None,
    }
}

/// Run many tool calls in one round-trip. Dispatches any built-in tool,
/// including bash (awaited per-call). One result block per call, in order.
/// ok only if every call succeeded.
/// Max concurrent inner calls inside `bulk`. Matches the default subagent
/// parallel fan-out so a single bulk doesn't stampede the host.
const BULK_CONCURRENCY: usize = 4;

/// Inner bulk calls that mutate workspace / shared state must run serially so
/// independent-looking batches cannot race two writes. Readonly + bash/fetch/
/// web_search (and other non-mutating tools) run concurrently.
fn bulk_must_serialize(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "edit"
            | "patch"
            | "delete"
            | "rename"
            | "mkdir"
            | "todo_write"
            | "git_add"
            | "git_commit"
            | "memory"
            | "bulk_write"
            | "bulk_edit"
    )
}

pub(crate) async fn dispatch_bulk_inner(name: &str, inner_args: &Value, cfg: &Config) -> Outcome {
    if name == "bash" {
        let cmd = inner_args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let timeout_override = inner_args.get("timeout").and_then(|v| v.as_u64());
        execute_bash(cmd, cfg, timeout_override, SudoAuth::None).await
    } else if name == "fetch" {
        execute_fetch(inner_args, cfg).await
    } else if name == "web_search" {
        execute_web_search(inner_args, cfg).await
    } else if name == "diagnostics" {
        execute_diagnostics(inner_args, cfg).await
    } else if crate::sandbox::is_sandbox_enabled()
        && matches!(
            name,
            "git_status" | "git_diff" | "git_log" | "git_add" | "git_commit"
        )
    {
        // Sandboxed: built-in git runs inside the microVM via the shared
        // execution backend (never directly on the host). The host path
        // (execute() -> git_exec) still applies when sandboxing is off.
        crate::tooling::builtin::git::git_dispatch(name, inner_args, cfg).await
    } else {
        // Sync tools: offload so concurrent bulk inners don't block the runtime.
        let name = name.to_string();
        let inner_args = inner_args.clone();
        let cfg = cfg.clone();
        match tokio::task::spawn_blocking(move || execute(&name, &inner_args, &cfg)).await {
            Ok(o) => o,
            Err(_) => Outcome::err("bulk inner task panicked"),
        }
    }
}

pub async fn execute_bulk(
    args: &Value,
    cfg: &Config,
    denied: &std::collections::HashMap<usize, String>,
) -> Outcome {
    let Some(calls) = args.get("calls").and_then(|v| v.as_array()) else {
        return Outcome::err("bulk requires a 'calls' array");
    };
    if calls.is_empty() {
        return Outcome::err("bulk requires a non-empty 'calls' array");
    }

    // Pre-resolve each slot: early errors stay in-order; the rest split into
    // concurrent vs serial waves so writes never race.
    let mut early: Vec<(usize, String)> = Vec::new();
    let mut concurrent: Vec<(usize, String, Value)> = Vec::new();
    let mut serial: Vec<(usize, String, Value)> = Vec::new();

    for (i, c) in calls.iter().enumerate() {
        let name = c
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let inner_args = c.get("args").cloned().unwrap_or(json!({}));
        // Caller-side gate (permission deny-rules + dangerous-path + plugin
        // pre-hooks) may have denied this inner call so destructive ops can't
        // evade the safety floor by hiding inside a bulk call. Render + skip.
        if let Some(msg) = denied.get(&i) {
            early.push((i, format!("### [{i}] {name}\n⚠ denied: {msg}")));
            continue;
        }
        if name.is_empty() {
            early.push((
                i,
                format!("### [{i}] <missing name>\nerror: missing 'name'"),
            ));
            continue;
        }
        // Nested bulk would recurse; block it to keep the gate simple.
        if name == "bulk" || name == "bulk_read" || name == "bulk_write" || name == "bulk_edit" {
            early.push((
                i,
                format!("### [{i}] {name}\nerror: nested bulk calls are not allowed"),
            ));
            continue;
        }
        if bulk_must_serialize(&name) {
            serial.push((i, name, inner_args));
        } else {
            concurrent.push((i, name, inner_args));
        }
    }

    let mut collected: Vec<(usize, String, bool)> = Vec::with_capacity(calls.len());
    for (i, block) in early {
        collected.push((i, block, false));
    }

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(BULK_CONCURRENCY));
    let mut handles: Vec<tokio::task::JoinHandle<(usize, String, Outcome)>> =
        Vec::with_capacity(concurrent.len());
    for (i, name, inner_args) in concurrent {
        let sem = sem.clone();
        let cfg = cfg.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok();
            let r = dispatch_bulk_inner(&name, &inner_args, &cfg).await;
            (i, name, r)
        }));
    }
    for h in handles {
        match h.await {
            Ok((i, name, r)) => {
                let ok = r.ok;
                collected.push((i, format!("### [{i}] {name}\n{}", r.output), ok));
            }
            Err(_) => collected.push((
                usize::MAX,
                "### [?]\nerror: bulk inner task panicked".into(),
                false,
            )),
        }
    }

    for (i, name, inner_args) in serial {
        let r = dispatch_bulk_inner(&name, &inner_args, cfg).await;
        let ok = r.ok;
        collected.push((i, format!("### [{i}] {name}\n{}", r.output), ok));
    }

    collected.sort_by_key(|(i, _, _)| *i);
    let ok = collected.iter().all(|(_, _, o)| *o);
    let blocks: Vec<String> = collected.into_iter().map(|(_, b, _)| b).collect();
    Outcome {
        ok,
        output: blocks.join("\n\n"),
        diff: None,
    }
}

// ---- todo / plan tracking (item 5) ----
// ponytail: a JSON file in .catalyst-code/todo.json in the workspace. No DB,
// no schema migration — just a list of {subject, status, content?}.

fn todo_path(cfg: &Config) -> std::path::PathBuf {
    cfg.workspace.join(".catalyst-code").join("todo.json")
}

fn todo_read(cfg: &Config) -> Outcome {
    let p = todo_path(cfg);
    match std::fs::read_to_string(&p) {
        Ok(s) => Outcome::ok(s),
        Err(_) => Outcome::ok("[]"), // empty plan, not an error
    }
}

fn todo_write(args: &Value, cfg: &Config) -> Outcome {
    let Some(todos) = args.get("todos").and_then(|v| v.as_array()) else {
        return Outcome::err("todo_write requires a 'todos' array");
    };
    // Validate shape: each must have subject + status.
    for (i, t) in todos.iter().enumerate() {
        let subject = t.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if subject.is_empty() {
            return Outcome::err(format!("todo #{i}: missing 'subject'"));
        }
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return Outcome::err(format!(
                "todo #{i}: status must be pending|in_progress|completed, got {status:?}"
            ));
        }
    }
    let p = todo_path(cfg);
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Outcome::err(format!("todo_write mkdir failed: {e}"));
        }
    }
    let body = json!({ "todos": todos });
    let pretty = serde_json::to_string_pretty(&body).unwrap_or_default();
    match atomic_write_file(&p, &pretty) {
        Ok(_) => Outcome::ok(format!("wrote {} todo(s)", todos.len())),
        Err(e) => Outcome::err(format!("todo_write failed: {e}")),
    }
}

// ---- unified diff / patch tool (item G) ----
// ponytail: hand-rolled unified-diff applier. Handles @@ hunk headers and
// context/add/remove lines. No rename/binary support; covers the common case
// of `diff --git a/... b/...` or bare hunks the model emits.

fn apply_patch(args: &Value, cfg: &Config) -> Outcome {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
    if path.is_empty() || patch.is_empty() {
        return Outcome::err("patch requires 'path' and 'patch'");
    }
    let resolved = match resolve_ws(cfg, path) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let original = std::fs::read_to_string(&resolved).unwrap_or_default();
    match apply_unified_diff(&original, patch) {
        Ok(new) => {
            if let Err(e) = atomic_write_file(&resolved, &new) {
                return Outcome::err(format!("patch write failed: {e}"));
            }
            let mut out = Outcome::ok(format!(
                "applied patch to {path} ({} -> {} bytes)",
                original.len(),
                new.len()
            ));
            out.diff = Some(make_unified_diff(&original, &new, path, 3));
            out
        }
        Err(e) => Outcome::err(format!("patch failed: {e}")),
    }
}

/// Apply a unified diff to `original`, returning the new text. Supports
/// multiple @@ hunk headers. Context lines must match.
fn apply_unified_diff(original: &str, patch: &str) -> Result<String, String> {
    let mut lines: Vec<String> = original.lines().map(String::from).collect();
    // Track trailing newline so we don't add/drop one spuriously.
    let had_trailing_nl = original.ends_with('\n');
    let mut i = 0;
    let patch_lines: Vec<&str> = patch.lines().collect();
    while i < patch_lines.len() {
        let l = patch_lines[i];
        // Skip file headers (--- / +++) and diff --git lines.
        if l.starts_with("---")
            || l.starts_with("+++")
            || l.starts_with("diff --git")
            || l.starts_with("Index:")
        {
            i += 1;
            continue;
        }
        if let Some(rest) = l.strip_prefix("@@") {
            // Parse @@ -start[,count] +start2[,count2] @@. We use the old start
            // to locate the hunk and the old count to guard against a malformed
            // hunk over-consuming source lines (which would silently mis-apply).
            let (old_start, old_count) = rest
                .split(' ')
                .find_map(|tok| {
                    tok.strip_prefix('-').and_then(|s| {
                        let mut parts = s.split(',');
                        let start = parts.next()?.parse::<usize>().ok()?;
                        let count = parts
                            .next()
                            .and_then(|n| n.parse::<usize>().ok())
                            .unwrap_or(1);
                        Some((start, count))
                    })
                })
                .ok_or_else(|| format!("bad hunk header: {l}"))?;
            i += 1;
            let mut target = if old_count == 0 {
                old_start
            } else {
                old_start.saturating_sub(1)
            };
            if old_count == 0 && target > lines.len() {
                return Err(format!(
                    "zero-count hunk insertion position {target} is past end of file ({})",
                    lines.len()
                ));
            }
            let mut consumed_old = 0usize;
            // Apply lines until the next hunk or EOF.
            while i < patch_lines.len() && !patch_lines[i].starts_with("@@") {
                let pl = patch_lines[i];
                if let Some(content) = pl.strip_prefix(' ') {
                    // context: must match. Past-EOF used to silently advance
                    // `target` and mis-apply later hunks; error instead.
                    // Empty-file / @@ -0,0 creates only use `+` lines, so they
                    // never hit this path.
                    if target >= lines.len() {
                        return Err(format!(
                            "context past end of file at line {}: {:?}",
                            target + 1,
                            content
                        ));
                    }
                    if lines[target] != content {
                        return Err(format!(
                            "context mismatch at line {}: expected {:?}, got {:?}",
                            target + 1,
                            lines[target],
                            content
                        ));
                    }
                    target += 1;
                    consumed_old += 1;
                } else if let Some(content) = pl.strip_prefix('-') {
                    // removal
                    if target >= lines.len() {
                        return Err(format!(
                            "removal past end of file at line {}: {:?}",
                            target + 1,
                            content
                        ));
                    }
                    if lines[target] == content {
                        lines.remove(target);
                    } else {
                        return Err(format!(
                            "removal mismatch at line {}: {:?} not found",
                            target + 1,
                            content
                        ));
                    }
                    consumed_old += 1;
                } else if let Some(content) = pl.strip_prefix('+') {
                    // addition. Clamp the insert index so a blank context line
                    // (below) that advanced `target` past the end can't make this
                    // insert panic with an out-of-bounds index (P1-1).
                    lines.insert(target.min(lines.len()), content.to_string());
                    target += 1;
                } else if pl.is_empty() {
                    // A truly-empty line is non-standard unified diff, but some
                    // tools emit it as a blank context line. Validate it matches
                    // an empty source line so a stray blank can't silently
                    // advance `target` past a real line and mis-apply the hunk.
                    // Past-EOF blank is allowed (then `+` inserts via clamp) —
                    // see patch_blank_line_in_hunk_no_panic.
                    // It is NOT counted toward `consumed_old` — the hunk header's
                    // count covers standard ` `/`-`/`+` lines, not these blanks.
                    if target < lines.len() && !lines[target].is_empty() {
                        return Err(format!("context mismatch at line {}: expected {:?}, got a blank (empty) context line", target + 1, lines[target]));
                    }
                    target += 1;
                } else {
                    // unknown line (\\ No newline, etc.) — skip
                }
                i += 1;
            }
            // Guard against over-consumption (a valid diff never consumes more
            // source lines than its header claims). Under-consumption is allowed
            // leniently to avoid rejecting quirky-but-valid patches.
            if old_count > 0 && consumed_old > old_count {
                return Err(format!("hunk @{old_start},{old_count} consumed {consumed_old} source lines (over-consumption — malformed patch?)"));
            }
            continue;
        }
        i += 1;
    }
    let mut out = lines.join("\n");
    if had_trailing_nl {
        out.push('\n');
    }
    Ok(out)
}

// ---- diagnostics (item 5) ----
// ponytail: detect the project type from marker files and run the right
// checker. Returns stdout+stderr. Async because it shells out.

pub async fn execute_diagnostics(args: &Value, cfg: &Config) -> Outcome {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let target = if path.is_empty() {
        cfg.workspace.clone()
    } else {
        match resolve_ws(cfg, path) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        }
    };
    // Pick checker by marker files present. The cargo/go checkers run a
    // program directly; the tsc-fallback and py_compile checkers run a small
    // shell pipeline, so their syntax must match the active shell (bash on
    // Unix, PowerShell on Windows) and is routed through `shell_argv` like
    // the `bash` tool so the OS gets matching semantics.
    let posix = crate::sandbox::policy::effective_shell_kind().is_posix();
    let (cmd, label): (Vec<String>, &str) = if target.join("Cargo.toml").exists() {
        (
            vec![
                "cargo".to_string(),
                "check".into(),
                "--message-format=short".into(),
            ],
            "cargo check",
        )
    } else if target.join("package.json").exists() {
        // try tsc, fall back to `npm run build` if no tsc.
        let script = if posix {
            "npx --no-install tsc --noEmit 2>&1 || npm run --silent build 2>&1".to_string()
        } else {
            "npx --no-install tsc --noEmit 2>&1; if ($LASTEXITCODE -ne 0) { npm run --silent build 2>&1 }".to_string()
        };
        let (prog, args) = shell_argv(&script);
        let mut cmd = vec![prog];
        cmd.extend(args);
        (cmd, "tsc/npm build")
    } else if target.join("go.mod").exists() {
        (
            vec!["go".to_string(), "build".into(), "./...".into()],
            "go build",
        )
    } else if target.join("pyproject.toml").exists() || target.join("setup.py").exists() {
        let script = if posix {
            "python -m py_compile $(find . -name '*.py' -not -path './.venv/*' | head -50) 2>&1"
                .to_string()
        } else {
            "Get-ChildItem -Path . -Recurse -Filter *.py | Where-Object { $_.FullName -notlike '*\\.venv*' } | Select-Object -First 50 | ForEach-Object { python -m py_compile $_.FullName } 2>&1".to_string()
        };
        let (prog, args) = shell_argv(&script);
        let mut cmd = vec![prog];
        cmd.extend(args);
        (cmd, "py_compile")
    } else {
        return Outcome::err(
            "no recognized project marker (Cargo.toml/package.json/go.mod/pyproject.toml)",
        );
    };
    let timeout = std::time::Duration::from_secs(cfg.diag_timeout_secs.max(5));
    // Resolve the workspace-relative path so the cwd is correct for the active
    // backend (host path on the host; /workspace/<rel> in the microVM).
    let rel = target
        .strip_prefix(&cfg.workspace)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let cwd =
        crate::sandbox::policy::effective_cwd(cfg, rel).unwrap_or_else(|_| cfg.workspace.clone());
    let proc_env = crate::sandbox::policy::build_process_env(
        cfg,
        crate::sandbox::policy::ExecPurpose::Diagnostics,
    );
    // Diagnostics runs a fixed checker (not model-controlled bash), so the
    // bash denylist doesn't apply. When sandboxed the checker runs in the
    // guest; a missing toolchain surfaces an actionable image error and NEVER
    // falls back to the host compiler.
    let req = crate::sandbox::ExecRequest {
        program: cmd[0].clone(),
        args: cmd[1..].to_vec(),
        cwd,
        env: proc_env.env,
        inherit_parent_env: proc_env.inherit_parent,
        stdin: None,
        timeout,
        ..Default::default()
    };
    let r = match crate::sandbox::execution_backend().execute(req).await {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("{label} failed: {}", e.user_message())),
    };
    if r.timed_out {
        return Outcome::err(format!(
            "{label} timed out after {}s (killed)",
            timeout.as_secs()
        ));
    }
    let mut s = String::new();
    if !r.stdout.is_empty() {
        s.push_str(&String::from_utf8_lossy(&r.stdout));
    }
    if !r.stderr.is_empty() {
        if !s.is_empty() {
            s.push_str("\n--- stderr ---\n");
        }
        s.push_str(&String::from_utf8_lossy(&r.stderr));
    }
    if s.is_empty() {
        s.push_str("(no diagnostics — clean)");
    }
    // Same 32KB smart_truncate CAP as bash — cargo/tsc/go dumps can be huge.
    const CAP: usize = 32_768;
    let mut output = format!("{label}\n{s}");
    if output.len() > CAP {
        output = smart_truncate(&output, CAP);
    }
    // Feed the checker's failure into the failure atlas — a non-zero exit
    // here is always a real diagnostic (compile/type/build error), so no
    // output-content gate is needed (unlike the bash tool).
    if r.exit_code != Some(0) {
        let pid = crate::project_identity::resolve_project_identity(&cfg.workspace).id;
        crate::failure_atlas::record_diagnostic(&pid, label, &output);
    }
    // ponytail: diagnostics "ok" is true only when the checker exits 0.
    Outcome {
        ok: r.exit_code == Some(0),
        output,
        diff: None,
    }
}

// ---- spawn (subagent) (item 8) ----
// ponytail: the spawn tool's body is in main.rs (it needs the reqwest client,
// api key, models, conversation). tools.rs just exposes the tool definition.
// execute() returns a sentinel so misuse surfaces clearly.

// ---- unified diff (display only) ----
// A compact LCS-based line diff for the TUI. Emitted as a separate `diff` event
// field (NOT in the model-facing output) so the model's tool-result stays small.
/// Build a unified diff between `old` and `new`, labeled with `path`, keeping
/// `context` lines around each change. Returns "" when byte-identical. Bounded:
/// falls back to a coarse summary for very large files and caps line output.
#[allow(clippy::needless_range_loop)]
pub fn make_unified_diff(old: &str, new: &str, path: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let (m, n) = (a.len(), b.len());
    // Guard: O(m*n) LCS is too expensive for huge files; emit a bounded note.
    if (m as u64) * (n as u64) > 4_000_000 {
        return format!(
            "--- a/{path}\n+++ b/{path}\n@@ -1,{m} +1,{n} @@\n… large change ({m} → {n} lines); diff omitted for size …"
        );
    }
    // LCS length table.
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Equal,
        Del,
        Ins,
    }
    let mut script: Vec<(Op, &str)> = Vec::with_capacity(m + n);
    let (mut i, mut j) = (0usize, 0usize);
    while i < m && j < n {
        if a[i] == b[j] {
            script.push((Op::Equal, a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            script.push((Op::Del, a[i]));
            i += 1;
        } else {
            script.push((Op::Ins, b[j]));
            j += 1;
        }
    }
    while i < m {
        script.push((Op::Del, a[i]));
        i += 1;
    }
    while j < n {
        script.push((Op::Ins, b[j]));
        j += 1;
    }
    let len = script.len();
    // Precompute old/new line counters before each entry (1-based).
    let mut ol_before = vec![0usize; len];
    let mut nl_before = vec![0usize; len];
    let (mut ol, mut nl) = (1usize, 1usize);
    for idx in 0..len {
        ol_before[idx] = ol;
        nl_before[idx] = nl;
        match script[idx].0 {
            Op::Equal => {
                ol += 1;
                nl += 1;
            }
            Op::Del => ol += 1,
            Op::Ins => nl += 1,
        }
    }
    // Mark kept lines: changed lines plus `context` around them.
    let mut keep = vec![false; len];
    for idx in 0..len {
        if script[idx].0 != Op::Equal {
            let lo = idx.saturating_sub(context);
            let hi = (idx + context).min(len - 1);
            for k in lo..=hi {
                keep[k] = true;
            }
        }
    }
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let cap = 4000usize;
    let mut emitted = 0usize;
    let mut idx = 0usize;
    while idx < len {
        if !keep[idx] {
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < len && keep[idx] {
            idx += 1;
        }
        let end = idx;
        let old_start = ol_before[start];
        let new_start = nl_before[start];
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        for k in start..end {
            match script[k].0 {
                Op::Equal => {
                    old_count += 1;
                    new_count += 1;
                }
                Op::Del => old_count += 1,
                Op::Ins => new_count += 1,
            }
        }
        out.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for k in start..end {
            if emitted >= cap {
                out.push_str(&format!("… (diff truncated; {m}→{n} lines) …\n"));
                return out;
            }
            match script[k].0 {
                Op::Equal => {
                    out.push(' ');
                    out.push_str(script[k].1);
                    out.push('\n');
                }
                Op::Del => {
                    out.push('-');
                    out.push_str(script[k].1);
                    out.push('\n');
                }
                Op::Ins => {
                    out.push('+');
                    out.push_str(script[k].1);
                    out.push('\n');
                }
            }
            emitted += 1;
        }
    }
    out
}

// ---- collections (RAG over arbitrary docs) ----

/// Document collections with embedding retrieval. Dispatches to
/// `crate::collections`. Reads the workspace for `add_file`/`index`; all
/// writes are to the harness learning dir (never the workspace), so the tool
/// is classified ReadOnly (like `memory`).
fn collections_tool(args: &Value, cfg: &Config) -> Outcome {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if action.is_empty() {
        return Outcome::err(
            "collections requires 'action' (add|add_file|index|search|list|stats|remove)",
        );
    }
    let pid = crate::project_identity::resolve_project_identity(&cfg.workspace).id;
    let collection = args
        .get("collection")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match action {
        "list" => {
            let cols = crate::collections::list(&pid);
            if cols.is_empty() {
                return Outcome::ok(
                    "No collections yet. Use `collections add` or `collections index` to create one.",
                );
            }
            let mut out = String::from("Collections:\n");
            for m in cols {
                out.push_str(&format!(
                    "- {} ({} chunks, {} sources, {} chars)\n",
                    m.name, m.chunk_count, m.source_count, m.total_chars
                ));
            }
            Outcome::ok(out.trim_end().to_string())
        }
        "stats" => {
            if collection.is_empty() {
                return Outcome::err("collections stats requires 'collection'");
            }
            match crate::collections::stats(&pid, collection) {
                Ok(m) => Outcome::ok(format!(
                    "Collection '{}' — {} chunks, {} sources, {} chars indexed.",
                    m.name, m.chunk_count, m.source_count, m.total_chars
                )),
                Err(e) => Outcome::err(e),
            }
        }
        "add" => {
            if collection.is_empty() {
                return Outcome::err("collections add requires 'collection'");
            }
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if text.trim().is_empty() {
                return Outcome::err("collections add requires 'text'");
            }
            let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("inline");
            match crate::collections::add_text(&pid, collection, text, source) {
                Ok(r) => Outcome::ok(format!(
                    "Indexed {} chars into '{}' ({} new chunks; {} total).",
                    r.chars_indexed, r.collection, r.chunks_added, r.total_chunks
                )),
                Err(e) => Outcome::err(e),
            }
        }
        "add_file" => {
            if collection.is_empty() {
                return Outcome::err("collections add_file requires 'collection'");
            }
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.trim().is_empty() {
                return Outcome::err("collections add_file requires 'path'");
            }
            let resolved = match resolve_ws(cfg, path) {
                Ok(p) => p,
                Err(e) => return Outcome::err(e),
            };
            let text = match std::fs::read_to_string(&resolved) {
                Ok(t) => t,
                Err(e) => return Outcome::err(format!("read {path:?} failed: {e}")),
            };
            if text.trim().is_empty() {
                return Outcome::err(format!("file {path:?} is empty"));
            }
            match crate::collections::add_text(&pid, collection, &text, path) {
                Ok(r) => Outcome::ok(format!(
                    "Indexed '{}' into '{}' ({} new chunks; {} total).",
                    path, r.collection, r.chunks_added, r.total_chunks
                )),
                Err(e) => Outcome::err(e),
            }
        }
        "index" => {
            if collection.is_empty() {
                return Outcome::err("collections index requires 'collection'");
            }
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.trim().is_empty() {
                return Outcome::err("collections index requires 'path' (directory)");
            }
            let resolved = match resolve_ws(cfg, path) {
                Ok(p) => p,
                Err(e) => return Outcome::err(e),
            };
            let exts: Option<Vec<String>> = args
                .get("exts")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
            match crate::collections::index_directory(&pid, collection, &resolved, exts.as_deref()) {
                Ok(r) => {
                    let mut out = format!(
                        "Indexed '{}' into '{}' — {} files, {} chunks, {} bytes ({} skipped).",
                        path, r.collection, r.files_indexed, r.chunks_added, r.bytes_indexed, r.files_skipped
                    );
                    if !r.skipped.is_empty() {
                        out.push_str("\nSkipped (sample):");
                        for s in &r.skipped {
                            out.push_str(&format!("\n  - {s}"));
                        }
                    }
                    Outcome::ok(out)
                }
                Err(e) => Outcome::err(e),
            }
        }
        "search" => {
            if collection.is_empty() {
                return Outcome::err("collections search requires 'collection'");
            }
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if query.trim().is_empty() {
                return Outcome::err("collections search requires 'query'");
            }
            let limit = args

                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(6);
            match crate::collections::search(&pid, collection, query, limit) {
                Ok(hits) if hits.is_empty() => Outcome::ok(format!(
                    "No matches in collection '{}' for that query. (Is it indexed? Try `collections stats {}`.)",
                    collection, collection
                )),
                Ok(hits) => {
                    let mut out = String::new();
                    for h in &hits {
                        out.push_str(&format!(
                            "[{:.2}] {} ({}:{})\n{}\n\n",
                            h.score, h.collection, h.source, h.chunk_id, h.text
                        ));
                    }
                    Outcome::ok(format!(
                        "Top {} hits in '{}':\n\n{}",
                        hits.len(),
                        collection,
                        out.trim_end()
                    ))
                }
                Err(e) => Outcome::err(e),
            }
        }
        "remove" => {
            if collection.is_empty() {
                return Outcome::err("collections remove requires 'collection'");
            }
            match crate::collections::remove(&pid, collection) {
                Ok(()) => Outcome::ok(format!("Removed collection '{}'.", collection)),
                Err(e) => Outcome::err(e),
            }
        }
        other => Outcome::err(format!(
            "unknown collections action '{other}'; expected add|add_file|index|search|list|stats|remove"
        )),
    }
}

// ---- git tools (shell out to the `git` binary; cwd = workspace) ----

/// Run `git` in the workspace and return its combined output. Bounded:
/// - stdin is null so a hook reading stdin can't hang the harness;
/// - a 30s deadline kills a stuck process (git tools run synchronously,
///   outside the /abort tokio::select, so we must self-limit);
/// - stdout/stderr are drained on threads so a large diff can't fill the
///   pipe buffer and deadlock the child while we poll for exit.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    fn tmp_ws() -> (PathBuf, Config) {
        // ponytail: unique dir per call via atomic counter — tests run in parallel and share temp otherwise.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("catalyst_code_tools_ws_{}", n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            workspace: dir.clone(),
            max_read_bytes: 1_048_576,
            max_read_lines: 2000,
            ..Config::default()
        };
        (dir, cfg)
    }

    #[test]
    fn read_file_returns_plain_content() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "alpha\nbeta\n").unwrap();
        let o = execute("read_file", &json!({"path":"f.txt"}), &cfg);
        assert!(o.ok, "{}", o.output);
        // Plain content: no hash/line-number prefix, exact bytes the model can copy.
        assert_eq!(o.output, "alpha\nbeta\n", "{}", o.output);
        assert!(
            !o.output.contains('│'),
            "should not contain a hash/line-number gutter"
        );
    }

    #[test]
    fn edit_replace_single_line() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "two", "replace": "TWO" }] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn edit_replace_multiline_insert_and_prepend() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "a\nb\nc\nd\n").unwrap();
        // replace a 2-line block, then append after 'd', then prepend before 'a'
        let edits = vec![
            json!({ "search": "b\nc", "replace": "X\nY" }),
            json!({ "search": "d", "replace": "d\nZ" }),
            json!({ "search": "a", "replace": "P\na" }),
        ];
        let args = json!({ "path": "f.txt", "edits": edits });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "P\na\nX\nY\nd\nZ\n"
        );
    }

    #[test]
    fn edit_delete_via_empty_replace() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "keep\nkill\nkeep2\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "kill\n", "replace": "" }] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "keep\nkeep2\n"
        );
    }

    #[test]
    fn edit_not_found_rejected() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "one\ntwo\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "nope", "replace": "x" }] });
        let o = execute("edit", &args, &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("not found"), "{}", o.output);
        // file unchanged
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn edit_ambiguous_rejected() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "dup\nx\ndup\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "dup", "replace": "DUP" }] });
        let o = execute("edit", &args, &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("2 places"), "{}", o.output);
    }

    #[test]
    fn edit_atomic_on_failure() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "one\ntwo\n").unwrap();
        // first edit would succeed, second fails -> nothing written
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "one", "replace": "ONE" },
            { "search": "missing", "replace": "x" }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(!o.ok);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "dup\nx\ndup\ndup\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "dup", "replace": "DUP", "replace_all": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "DUP\nx\nDUP\nDUP\n"
        );
    }

    #[test]
    fn edit_normalize_whitespace_tolerates_drift() {
        let (_root, cfg) = tmp_ws();
        // file uses tabs + extra spaces; search uses single spaces
        fs::write(
            cfg.workspace.join("f.txt"),
            "fn  main() {\n\tif (x)  return;\n}\n",
        )
        .unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "if (x) return;", "replace": "if (x) { return; }", "normalize_whitespace": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "fn  main() {\n\tif (x) { return; }\n}\n"
        );
    }

    #[test]
    fn edit_normalize_whitespace_replace_all() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "a   b\na\tb\na b\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "a b", "replace": "X", "normalize_whitespace": true, "replace_all": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "X\nX\nX\n"
        );
    }

    #[test]
    fn edit_normalize_whitespace_multibyte_no_corruption() {
        // C1 regression: normalize_whitespace matching indexed a per-char map
        // with a BYTE offset from str::find. For multi-byte content (CJK,
        // emoji, smart quotes) this either panicked (OOB) or returned the wrong
        // span and silently corrupted the file via replace_range. The fix
        // tracks byte offset + char index in parallel.
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "漢字\n\tif (x)  return;\n}\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "if (x) return;", "replace": "if (x) { return; }", "normalize_whitespace": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "漢字\n\tif (x) { return; }\n}\n"
        );
    }

    #[test]
    fn edit_normalize_whitespace_multibyte_replace_all() {
        // Same class of bug, replace_all path: each match's span must map back
        // to the correct source bytes even when the collapsed string contains
        // multi-byte chars between matches.
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "→ a   b\n★ a\tb\n☆ a b\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "a b", "replace": "X", "normalize_whitespace": true, "replace_all": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "→ X\n★ X\n☆ X\n"
        );
    }

    #[test]
    fn edit_not_found_gives_closest_hint() {
        let (_root, cfg) = tmp_ws();
        fs::write(
            cfg.workspace.join("f.txt"),
            "alpha beta gamma\ndelta epsilon\n",
        )
        .unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "alpha gamma", "replace": "x" }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("closest match"), "{}", o.output);
        assert!(o.output.contains("line 1"), "{}", o.output);
    }

    #[test]
    fn edit_replace_all_does_not_clobber_inserted_text() {
        // Regression: a single edit call with BOTH (a) an insertion whose
        // replace text contains substring S, AND (b) a replace_all whose search
        // is S, would silently rewrite the just-inserted text (the replace_all
        // runs after the insertion on the evolving content). This must now
        // fail-closed with a clear error instead of silently corrupting.
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "foo()\nbar()\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "bar()", "replace": "bar()\n// helper: foo() is great" },
            { "search": "foo()", "replace": "qux()", "replace_all": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(
            !o.ok,
            "should fail-closed, not silently clobber; got: {}",
            o.output
        );
        assert!(
            o.output.contains("replace_all"),
            "error should explain the conflict: {}",
            o.output
        );
        assert!(
            o.output.contains("foo()"),
            "error should name the clobbered substring: {}",
            o.output
        );
        // File must be left untouched (atomic — no partial application).
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "foo()\nbar()\n"
        );
    }

    #[test]
    fn edit_normalize_whitespace_noop_multiline_no_corruption() {
        // Regression: normalize_whitespace:true on a multi-line no-op edit
        // (search == replace) must leave the file byte-identical. The earlier
        // per-char/byte indexing bug could corrupt even no-op edits by merging
        // adjacent lines and dropping tokens.
        let (_root, cfg) = tmp_ws();
        let original = "msg := fmt.Sprintf(\"hi\")\n\treturn lipgloss.NewStyle()\n";
        fs::write(cfg.workspace.join("f.txt"), original).unwrap();
        let args = json!({ "path": "f.txt", "edits": [
            { "search": "return lipgloss.NewStyle()", "replace": "return lipgloss.NewStyle()", "normalize_whitespace": true }
        ] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            original,
            "no-op normalize_whitespace edit must not corrupt multi-line content"
        );
    }

    #[test]
    fn preview_diff_edit_does_not_write() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "one\ntwo\n").unwrap();
        let edits = vec![json!({ "search": "one", "replace": "ONE" })];
        let diff = preview_diff_edit("f.txt", &edits, &cfg).unwrap();
        assert!(diff.contains("-one"), "{}", diff);
        assert!(diff.contains("+ONE"), "{}", diff);
        // file untouched — preview never writes
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn preview_diff_write_shows_new_file_as_addition() {
        let (_root, cfg) = tmp_ws();
        let diff = preview_diff_write("new.txt", "hello\n", &cfg).unwrap();
        assert!(diff.contains("+hello"), "{}", diff);
        assert!(diff.contains("+++ b/new.txt"), "{}", diff);
    }

    #[test]
    fn smart_truncate_keeps_tail_and_salvages_errors() {
        // build output: many plain head lines, an error line, more plain lines, a tail line
        let mut head = String::new();
        for _ in 0..2000 {
            head.push_str("line of build log\n");
        }
        head.push_str("error[E0308]: mismatched types\n");
        for _ in 0..2000 {
            head.push_str("more log\n");
        }
        head.push_str("final tail line here\n");
        let out = smart_truncate(&head, 4096);
        assert!(out.contains("final tail line here"), "tail must survive");
        assert!(
            out.contains("error[E0308]"),
            "error line from head must be salvaged"
        );
        assert!(
            out.contains("salvaged"),
            "must note salvaged lines: {}",
            out
        );
    }

    #[test]
    fn edit_empty_search_rejected() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "a\n").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "", "replace": "b" }] });
        let o = execute("edit", &args, &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("empty"), "{}", o.output);
    }

    #[test]
    fn edit_preserves_no_trailing_newline() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "a\nb").unwrap();
        let args = json!({ "path": "f.txt", "edits": [{ "search": "b", "replace": "b\nc" }] });
        let o = execute("edit", &args, &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("f.txt")).unwrap(),
            "a\nb\nc"
        );
    }

    #[test]
    fn workspace_confines_paths() {
        let (_root, cfg) = tmp_ws();
        // absolute rejected
        let o = execute("read_file", &json!({"path":"/etc/hostname"}), &cfg);
        assert!(!o.ok);
        // .. rejected
        let o = execute("read_file", &json!({"path":"../escape"}), &cfg);
        assert!(!o.ok);
        // inside ok
        fs::write(cfg.workspace.join("inside.txt"), "ok").unwrap();
        let o = execute("read_file", &json!({"path":"inside.txt"}), &cfg);
        assert!(o.ok, "{}", o.output);
    }

    #[test]
    fn never_mode_disables_path_confinement() {
        // Under Approval::Never ALL file restrictions are disabled: absolute
        // paths and `..` traversal are allowed (the model is fully trusted), so
        // path confinement is OFF — not just the dangerous-path list. This is
        // the counterpart to `workspace_confines_paths` (which asserts the
        // Destructive rejection of the same paths).
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let (_root, mut cfg) = tmp_ws();
        cfg.approval = crate::config::Approval::Never;
        // A small file in the PARENT of the workspace, reached both by absolute
        // path and by `..` traversal from inside the workspace.
        let parent = cfg.workspace.parent().unwrap().to_path_buf();
        let name = format!("catalyst_code_never_out_{n}.txt");
        let outside = parent.join(&name);
        fs::write(&outside, "leaked").unwrap();
        // Absolute path: allowed under Never (rejected under Destructive).
        let o = execute(
            "read_file",
            &json!({ "path": outside.to_str().unwrap() }),
            &cfg,
        );
        assert!(
            o.ok,
            "absolute read must be allowed under Never: {}",
            o.output
        );
        assert!(o.output.contains("leaked"), "{}", o.output);
        // `..` traversal: allowed under Never.
        let o = execute("read_file", &json!({ "path": format!("../{name}") }), &cfg);
        assert!(o.ok, "`..` read must be allowed under Never: {}", o.output);
        assert!(o.output.contains("leaked"), "{}", o.output);
        let _ = fs::remove_file(&outside);
    }

    #[test]
    fn read_file_size_guard() {
        let (_root, cfg) = tmp_ws();
        let big = "x".repeat((cfg.max_read_bytes + 100) as usize);
        fs::write(cfg.workspace.join("big.txt"), &big).unwrap();
        let o = execute("read_file", &json!({"path":"big.txt"}), &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("max"), "{}", o.output);
    }

    #[test]
    fn glob_matches_double_star() {
        let (root, cfg) = tmp_ws();
        fs::create_dir_all(root.join("src/a")).unwrap();
        fs::write(root.join("src/a/main.rs"), "x").unwrap();
        fs::write(root.join("src/lib.rs"), "x").unwrap();
        let o = execute("glob", &json!({"pattern":"**/*.rs"}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("main.rs"));
        assert!(o.output.contains("lib.rs"));
    }

    #[test]
    fn glob_matches_brace_alternatives() {
        let (root, cfg) = tmp_ws();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "x").unwrap();
        fs::write(root.join("src/lib.go"), "x").unwrap();
        fs::write(root.join("README.md"), "x").unwrap();
        fs::write(root.join("notes.txt"), "x").unwrap();
        let o = execute("glob", &json!({"pattern":"**/*.{rs,go,md}"}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("main.rs"), "{}", o.output);
        assert!(o.output.contains("lib.go"), "{}", o.output);
        assert!(o.output.contains("README.md"), "{}", o.output);
        assert!(!o.output.contains("notes.txt"), "{}", o.output);

        let g = execute("grep", &json!({"pattern":"x", "glob":"**/*.{rs,md}"}), &cfg);
        assert!(g.ok, "{}", g.output);
        assert!(g.output.contains("main.rs"), "{}", g.output);
        assert!(g.output.contains("README.md"), "{}", g.output);
        assert!(!g.output.contains("lib.go"), "{}", g.output);
    }

    #[test]
    fn grep_finds_matches() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        fs::write(root.join("b.txt"), "beta again\n").unwrap();
        let o = execute("grep", &json!({"pattern":"beta"}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:2:beta"));
        assert!(o.output.contains("b.txt:1:beta again"));
    }

    #[test]
    fn grep_context_surrounds_matches() {
        let (root, cfg) = tmp_ws();
        // Two matches 5 lines apart in one file; context=1 windows must not overlap
        // so a '...' separator appears between them.
        fs::write(
            root.join("a.txt"),
            "l1\nl2\nMARK\nl4\nl5\nl6\nl7\nMARK\nl9\n",
        )
        .unwrap();
        let o = execute("grep", &json!({"pattern":"MARK", "context": 1}), &cfg);
        assert!(o.ok, "{}", o.output);
        // match line uses ':' before the line number
        assert!(
            o.output.contains("a.txt:3:MARK"),
            "match marker: {}",
            o.output
        );
        assert!(
            o.output.contains("a.txt:8:MARK"),
            "match marker: {}",
            o.output
        );
        // context lines use '-' as the separator (GNU grep -C convention)
        assert!(
            o.output.contains("a.txt-2-l2"),
            "context marker: {}",
            o.output
        );
        assert!(
            o.output.contains("a.txt-4-l4"),
            "context marker: {}",
            o.output
        );
        // windows 5 apart (line 3 +/-1 and line 8 +/-1) do not overlap -> '...' between
        assert!(o.output.contains("..."), "group separator: {}", o.output);
    }

    #[test]
    fn grep_context_merges_overlapping_windows() {
        let (root, cfg) = tmp_ws();
        // Two adjacent matches at lines 3 and 4 with context=2 → one merged window, no '...'
        fs::write(root.join("a.txt"), "l1\nl2\nMARK\nMARK\nl5\nl6\n").unwrap();
        let o = execute("grep", &json!({"pattern":"MARK", "context": 2}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:3:MARK"));
        assert!(o.output.contains("a.txt:4:MARK"));
        assert!(
            !o.output.contains("..."),
            "merged window should have no separator: {}",
            o.output
        );
    }

    #[test]
    fn grep_context_clamps_at_file_edges() {
        let (root, cfg) = tmp_ws();
        // Match on line 1 with context=5 must clamp to the file start (no line 0).
        fs::write(root.join("a.txt"), "MARK\nl2\nl3\n").unwrap();
        let o = execute("grep", &json!({"pattern":"MARK", "context": 5}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:1:MARK"));
        assert!(o.output.contains("a.txt-2-l2"));
        assert!(o.output.contains("a.txt-3-l3"));
        // no negative/zero line numbers leaked
        assert!(!o.output.contains("a.txt-0-"));
        assert!(!o.output.contains("a.txt:0:"));
    }

    #[test]
    fn grep_context_zero_matches_legacy_format() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        // context omitted (default 0) → original one-line-per-match format, no '-'/'...'
        let o = execute("grep", &json!({"pattern":"beta"}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(o.output, "a.txt:2:beta");
    }

    #[test]
    fn grep_case_insensitive_and_glob_and_type() {
        let (root, cfg) = tmp_ws();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "Hello\n").unwrap();
        fs::write(root.join("src/b.txt"), "hello\n").unwrap();
        let o = execute(
            "grep",
            &json!({
                "pattern": "hello",
                "case_insensitive": true,
                "glob": "**/*.rs",
            }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.rs"));
        assert!(!o.output.contains("b.txt"));

        let o2 = execute("grep", &json!({ "pattern": "Hello", "type": "rs" }), &cfg);
        assert!(o2.ok, "{}", o2.output);
        assert!(o2.output.contains("a.rs"));
        assert!(!o2.output.contains("b.txt"));
    }

    #[test]
    fn grep_output_modes_files_and_count() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "x\nx\n").unwrap();
        fs::write(root.join("b.txt"), "x\n").unwrap();
        let files = execute(
            "grep",
            &json!({ "pattern": "x", "output_mode": "files_with_matches" }),
            &cfg,
        );
        assert!(files.ok, "{}", files.output);
        assert!(files.output.contains("a.txt"));
        assert!(files.output.contains("b.txt"));
        assert!(!files.output.contains(":1:"));

        let count = execute(
            "grep",
            &json!({ "pattern": "x", "output_mode": "count" }),
            &cfg,
        );
        assert!(count.ok, "{}", count.output);
        assert!(count.output.contains("a.txt:2"));
        assert!(count.output.contains("b.txt:1"));
        assert!(count.output.contains("# total: 3"));
    }

    #[test]
    fn grep_head_limit_and_offset() {
        let (root, cfg) = tmp_ws();
        let body: String = (1..=20).map(|n| format!("match{n}\n")).collect();
        fs::write(root.join("a.txt"), &body).unwrap();
        let o = execute(
            "grep",
            &json!({ "pattern": "match", "head_limit": 3, "offset": 2 }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("match3"));
        assert!(o.output.contains("match5"));
        assert!(!o.output.contains("match2\n") && !o.output.contains(":2:match2"));
        assert!(o.output.contains("offset=5") || o.output.contains("cap reached"));
    }

    #[test]
    fn grep_invert_excludes_matching_lines() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        // Directory search exercises the rg path; -v emits non-matching lines.
        let o = execute("grep", &json!({"pattern":"beta","invert":true}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:1:alpha"), "{}", o.output);
        assert!(o.output.contains("a.txt:3:gamma"), "{}", o.output);
        assert!(!o.output.contains(":2:beta"), "{}", o.output);
    }

    #[test]
    fn grep_invert_single_file_pure_rust() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        // Single-file path uses the pure-Rust walker (no rg spawn).
        let o = execute(
            "grep",
            &json!({"pattern":"beta","path":"a.txt","invert":true}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:1:alpha"), "{}", o.output);
        assert!(o.output.contains("a.txt:3:gamma"), "{}", o.output);
        assert!(!o.output.contains(":2:beta"), "{}", o.output);
    }

    #[test]
    fn grep_invert_files_without_match_is_dash_l() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "match\nother\n").unwrap();
        fs::write(root.join("b.txt"), "nope\nother\n").unwrap();
        // files_with_matches + invert == grep -L: files with NO match.
        let o = execute(
            "grep",
            &json!({"pattern":"match","output_mode":"files_with_matches","invert":true}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("b.txt"), "{}", o.output);
        assert!(!o.output.contains("a.txt"), "{}", o.output);
    }

    #[test]
    fn grep_invert_count_mode() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "x\ny\nz\n").unwrap();
        // count + invert: 3 lines total, 1 matches 'y' → 2 non-matching.
        let o = execute(
            "grep",
            &json!({"pattern":"y","output_mode":"count","invert":true}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.txt:2"), "{}", o.output);
    }

    #[test]
    fn grep_fixed_string_matches_literal_dots() {
        let (root, cfg) = tmp_ws();
        // 'a.b' as regex would match 'axb' too; -F must match the literal only.
        fs::write(root.join("a.txt"), "a.b\naxb\n").unwrap();
        let o = execute("grep", &json!({"pattern":"a.b","fixed_string":true}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains(":1:a.b"), "{}", o.output);
        assert!(!o.output.contains("axb"), "{}", o.output);
    }

    #[test]
    fn grep_word_match() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "cat\ncategory\nconcatenate\n").unwrap();
        let o = execute("grep", &json!({"pattern":"cat","word":true}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains(":1:cat"), "{}", o.output);
        assert!(!o.output.contains("category"), "{}", o.output);
        assert!(!o.output.contains("concatenate"), "{}", o.output);
    }

    #[test]
    fn grep_after_context_asymmetric() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "l1\nMARK\nl3\nl4\n").unwrap();
        // after:1 (no before) → match + 1 line after only.
        let o = execute("grep", &json!({"pattern":"MARK","after":1}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains(":2:MARK"), "{}", o.output);
        assert!(o.output.contains("a.txt-3-l3"), "{}", o.output);
        assert!(
            !o.output.contains("l1"),
            "no before-line expected: {}",
            o.output
        );
    }

    #[test]
    fn grep_before_context_single_file_pure_rust() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "l1\nl2\nMARK\nl4\n").unwrap();
        // Single-file path exercises the pure-Rust context renderer with before only.
        let o = execute(
            "grep",
            &json!({"pattern":"MARK","path":"a.txt","before":1}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains(":3:MARK"), "{}", o.output);
        assert!(o.output.contains("a.txt-2-l2"), "{}", o.output);
        assert!(
            !o.output.contains("l4"),
            "no after-line expected: {}",
            o.output
        );
    }

    #[test]
    fn grep_context_and_after_compose() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.txt"), "b0\nb1\nMARK\na1\na2\n").unwrap();
        // context:2 alone == -C2; after:1 should not shrink the before side below 2.
        let o = execute(
            "grep",
            &json!({"pattern":"MARK","context":2,"after":1}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains(":3:MARK"), "{}", o.output);
        assert!(
            o.output.contains("b0"),
            "before side should keep context:2: {}",
            o.output
        );
    }

    #[test]
    fn grep_glob_array_excludes_with_negation() {
        let (root, cfg) = tmp_ws();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "match\n").unwrap();
        fs::write(root.join("src/a_test.rs"), "match\n").unwrap();
        fs::write(root.join("src/b.rs"), "match\n").unwrap();
        // Directory search (rg path): include **/*.rs, exclude **/*test*.
        let o = execute(
            "grep",
            &json!({"pattern":"match","glob":["**/*.rs","!**/*test*"]}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.rs"), "{}", o.output);
        assert!(o.output.contains("b.rs"), "{}", o.output);
        assert!(!o.output.contains("a_test.rs"), "{}", o.output);
    }

    #[test]
    fn grep_glob_string_still_works() {
        // Backward compat: glob as a plain string (not array).
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.rs"), "match\n").unwrap();
        fs::write(root.join("a.txt"), "match\n").unwrap();
        let o = execute("grep", &json!({"pattern":"match","glob":"**/*.rs"}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.rs"), "{}", o.output);
        assert!(!o.output.contains("a.txt"), "{}", o.output);
    }

    #[test]
    fn grep_paths_multi_file() {
        let (root, cfg) = tmp_ws();
        fs::write(root.join("a.go"), "match\n").unwrap();
        fs::write(root.join("b.go"), "match\n").unwrap();
        fs::write(root.join("c.go"), "match\n").unwrap();
        // paths[] forces the pure-Rust walker (no rg) and searches exactly these files.
        let o = execute(
            "grep",
            &json!({"pattern":"match","paths":["a.go","b.go"]}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.go"), "{}", o.output);
        assert!(o.output.contains("b.go"), "{}", o.output);
        assert!(!o.output.contains("c.go"), "{}", o.output);
    }

    #[test]
    fn grep_paths_dir_with_glob_negation_pure_rust() {
        let (root, cfg) = tmp_ws();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "match\n").unwrap();
        fs::write(root.join("src/a_test.rs"), "match\n").unwrap();
        fs::write(root.join("src/b.rs"), "match\n").unwrap();
        // paths=[dir] skips rg → exercises the pure-Rust walker's glob_filter_passes.
        let o = execute(
            "grep",
            &json!({"pattern":"match","paths":["src"],"glob":["**/*.rs","!**/*test*"]}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("a.rs"), "{}", o.output);
        assert!(o.output.contains("b.rs"), "{}", o.output);
        assert!(!o.output.contains("a_test.rs"), "{}", o.output);
    }

    #[test]
    fn read_file_auto_windows_large_files() {
        let (_root, cfg) = tmp_ws();
        let body: String = (1..=600).map(|n| format!("line {n}\n")).collect();
        fs::write(cfg.workspace.join("big.txt"), &body).unwrap();
        let o = execute("read_file", &json!({ "path": "big.txt" }), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("auto-windowed"), "{}", o.output);
        assert!(o.output.contains("line 1"));
        assert!(o.output.contains("line 200"));
        assert!(!o.output.contains("line 201"));
    }

    #[test]
    fn read_file_line_numbers() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("a.txt"), "alpha\nbeta\n").unwrap();
        let o = execute(
            "read_file",
            &json!({ "path": "a.txt", "line_numbers": true }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("1|alpha"), "{}", o.output);
        assert!(o.output.contains("2|beta"), "{}", o.output);
    }

    #[test]
    fn delete_rename_mkdir_roundtrip() {
        let (root, cfg) = tmp_ws();
        let o = execute("mkdir", &json!({ "path": "sub/dir" }), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(root.join("sub/dir").is_dir());

        fs::write(root.join("sub/dir/f.txt"), "hi").unwrap();
        let o = execute(
            "rename",
            &json!({ "from": "sub/dir/f.txt", "to": "sub/dir/g.txt" }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(root.join("sub/dir/g.txt").is_file());
        assert!(!root.join("sub/dir/f.txt").exists());

        let o = execute("delete", &json!({ "path": "sub/dir/g.txt" }), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(!root.join("sub/dir/g.txt").exists());

        // non-empty dir refused
        fs::write(root.join("sub/dir/keep.txt"), "x").unwrap();
        let o = execute("delete", &json!({ "path": "sub/dir" }), &cfg);
        assert!(!o.ok, "{}", o.output);
    }

    #[tokio::test]
    async fn bash_timeout_kills() {
        let (_root, cfg) = tmp_ws();
        let mut cfg = cfg;
        cfg.bash_timeout_secs = 1;
        let o = execute_bash("sleep 30", &cfg, None, SudoAuth::None).await;
        assert!(!o.ok);
        assert!(o.output.contains("timed out"), "{}", o.output);
    }

    #[tokio::test]
    async fn bash_denylist_blocks() {
        let (_root, cfg) = tmp_ws();
        let o = execute_bash("rm -rf /", &cfg, None, SudoAuth::None).await;
        assert!(!o.ok);
        assert!(o.output.contains("denylist"), "{}", o.output);
    }

    #[tokio::test]
    async fn bash_runs_in_workspace() {
        let (root, cfg) = tmp_ws();
        let o = execute_bash("pwd", &cfg, None, SudoAuth::None).await;
        assert!(o.ok, "{}", o.output);
        // canonicalize both for comparison (tmp may be a symlink)
        assert_eq!(
            std::fs::canonicalize(o.output.trim()).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
    }

    #[tokio::test]
    async fn bulk_read_write_edit_roundtrip() {
        let (_root, cfg) = tmp_ws();
        // bulk_write three files
        let w = bulk_write(
            &json!({ "files": [
            { "path": "a.txt", "content": "alpha\nbeta\n" },
            { "path": "sub/b.txt", "content": "one\ntwo\n" },
            { "path": "c.txt", "content": "x\ny\nz\n" }
        ] }),
            &cfg,
        );
        assert!(w.ok, "{}", w.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("sub/b.txt")).unwrap(),
            "one\ntwo\n"
        );

        // bulk_read them back; middle file via plain content
        let r = bulk_read(&json!({ "paths": ["a.txt","sub/b.txt","nope.txt"] }), &cfg);
        assert!(!r.ok, "per-file error should mark batch not-ok");
        assert!(r.output.contains("alpha"), "{}", r.output);
        assert!(r.output.contains("### [2] nope.txt"), "{}", r.output);

        // bulk_edit: replace 'alpha' in a.txt, append 'END' after 'z' in c.txt
        let e = bulk_edit(
            &json!({ "edits": [
            { "path": "a.txt", "edits": [{ "search": "alpha", "replace": "ALPHA" }] },
            { "path": "c.txt", "edits": [{ "search": "z", "replace": "z\nEND" }] }
        ] }),
            &cfg,
        );
        assert!(e.ok, "{}", e.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("a.txt")).unwrap(),
            "ALPHA\nbeta\n"
        );
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("c.txt")).unwrap(),
            "x\ny\nz\nEND\n"
        );
    }

    #[tokio::test]
    async fn bulk_dispatches_bash_and_read() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("f.txt"), "hello\n").unwrap();
        let o = execute_bulk(&json!({ "calls": [ { "name": "read_file", "args": { "path": "f.txt" } }, { "name": "bash", "args": { "command": "echo hi" } } ] }), &cfg, &std::collections::HashMap::new()).await;
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("hello"), "{}", o.output);
        assert!(o.output.contains("hi"), "{}", o.output);
    }

    #[tokio::test]
    async fn bulk_rejects_nested_bulk() {
        let (_root, cfg) = tmp_ws();
        let o = execute_bulk(
            &json!({ "calls": [
            { "name": "bulk_read", "args": { "paths": ["f.txt"] } }
        ] }),
            &cfg,
            &std::collections::HashMap::new(),
        )
        .await;
        assert!(!o.ok);
        assert!(o.output.contains("nested bulk"), "{}", o.output);
    }

    #[tokio::test]
    async fn bulk_runs_independent_calls_concurrently() {
        // Four independent sleeps: sequential ≈ 600ms, concurrent ≈ 150ms.
        let (_root, cfg) = tmp_ws();
        let t0 = std::time::Instant::now();
        let o = execute_bulk(
            &json!({
                "calls": [
                    { "name": "bash", "args": { "command": "sleep 0.15" } },
                    { "name": "bash", "args": { "command": "sleep 0.15" } },
                    { "name": "bash", "args": { "command": "sleep 0.15" } },
                    { "name": "bash", "args": { "command": "sleep 0.15" } }
                ]
            }),
            &cfg,
            &std::collections::HashMap::new(),
        )
        .await;
        let elapsed = t0.elapsed();
        assert!(o.ok, "{}", o.output);
        assert!(
            elapsed.as_millis() < 450,
            "expected concurrent bulk (~150ms), got {elapsed:?}"
        );
        // Output blocks stay index-ordered even when futures finish out of order.
        let pos0 = o.output.find("### [0] bash").expect("slot 0");
        let pos3 = o.output.find("### [3] bash").expect("slot 3");
        assert!(pos0 < pos3);
    }

    #[test]
    fn todo_write_then_read_roundtrip() {
        let (_root, cfg) = tmp_ws();
        let o = execute(
            "todo_write",
            &json!({ "todos": [
            { "subject": "step 1", "status": "completed" },
            { "subject": "step 2", "status": "in_progress", "content": "detail" }
        ] }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        let r = execute("todo_read", &json!({}), &cfg);
        assert!(r.ok);
        assert!(r.output.contains("step 1"));
        assert!(r.output.contains("in_progress"));
        // bad status rejected
        let bad = execute(
            "todo_write",
            &json!({ "todos": [ { "subject": "x", "status": "bogus" } ] }),
            &cfg,
        );
        assert!(!bad.ok);
    }

    #[test]
    fn patch_applies_unified_diff() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("p.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let diff = "@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        let o = execute("patch", &json!({ "path": "p.txt", "patch": diff }), &cfg);
        assert!(o.ok, "{}", o.output);
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("p.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[test]
    fn patch_zero_count_insertions_follow_unified_diff_positions() {
        assert_eq!(
            apply_unified_diff("a\nb\n", "@@ -0,0 +1,1 @@\n+start\n").unwrap(),
            "start\na\nb\n"
        );
        assert_eq!(
            apply_unified_diff("a\nb\nc\n", "@@ -2,0 +3,1 @@\n+middle\n").unwrap(),
            "a\nb\nmiddle\nc\n"
        );
        assert_eq!(
            apply_unified_diff("", "@@ -0,0 +1,1 @@\n+only\n").unwrap(),
            "only"
        );
        assert!(apply_unified_diff("a\n", "@@ -2,0 +3,1 @@\n+bad\n").is_err());
    }

    #[test]
    fn patch_rejects_context_mismatch() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("p.txt"), "alpha\nbeta\n").unwrap();
        let diff = "@@ -1,2 +1,2 @@\n WRONG\n-beta\n+BETA\n";
        let o = execute("patch", &json!({ "path": "p.txt", "patch": diff }), &cfg);
        assert!(!o.ok);
        assert!(o.output.contains("context mismatch"), "{}", o.output);
    }

    #[test]
    fn read_file_pagination_window() {
        let (_root, cfg) = tmp_ws();
        let body: String = (1..=500).map(|n| format!("line {n}\n")).collect();
        fs::write(cfg.workspace.join("big.txt"), &body).unwrap();
        let o = execute(
            "read_file",
            &json!({ "path": "big.txt", "offset": 10, "limit": 3 }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("lines 10-12 of 500"), "{}", o.output);
        assert!(o.output.contains("line 10"));
        assert!(o.output.contains("line 12"));
        assert!(!o.output.contains("line 13"));
    }

    #[test]
    fn core_vs_deferred_tool_partition() {
        assert!(is_core_tool("read_file"));
        assert!(is_core_tool("bash"));
        assert!(is_core_tool("load_tools"));
        assert!(is_core_tool("subagent"));
        assert!(!is_core_tool("fetch"));
        assert!(is_deferred_tool("fetch"));
        assert!(is_deferred_tool("git_status"));
        assert!(is_deferred_tool("bulk_read"));
        assert!(!is_deferred_tool("read_file"));
        assert!(is_builtin("load_tools"));
        let defs = definitions();
        let names: Vec<_> = defs
            .iter()
            .filter_map(|d| {
                d.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
            })
            .collect();
        assert!(names.contains(&"load_tools"));
        assert!(names.contains(&"fetch"));
        // Every deferred tool must appear in definitions (so load_tools can
        // re-add its schema) and must NOT be core.
        for d in deferred_tool_names() {
            assert!(is_deferred_tool(d), "{d}");
            assert!(!is_core_tool(d), "{d} must not be core");
            assert!(names.contains(d), "{d} missing from definitions");
        }
    }

    #[test]
    fn finish_returns_sentinel() {
        let (_root, cfg) = tmp_ws();
        let o = execute("finish", &json!({}), &cfg);
        assert!(o.ok);
        assert_eq!(o.output, FINISH_SENTINEL);
    }

    #[test]
    fn write_file_primitive_no_longer_blocks_restricted_paths() {
        // The dangerous-path blocklist moved OUT of the primitive to the approval
        // gate (main::restricted_path_for_tool). So the primitive itself never
        // hard-blocks: under Approval::Never (or after an explicit approval) a
        // restricted file like .git/config CAN be written. This test pins that
        // contract so the enforcement doesn't silently migrate back here.
        let (_root, cfg) = tmp_ws();
        let o = execute(
            "write_file",
            &json!({"path":".git/config","content":"x"}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(cfg.workspace.join(".git/config").exists());
    }

    #[test]
    fn write_file_missing_content_errors_and_leaves_file() {
        // H5: omit `content` → tool error; existing file must not be emptied.
        let (_root, cfg) = tmp_ws();
        let path = cfg.workspace.join("keep.txt");
        fs::write(&path, "precious").unwrap();
        let o = execute("write_file", &json!({"path": "keep.txt"}), &cfg);
        assert!(!o.ok, "missing content must fail: {}", o.output);
        assert!(
            o.output.contains("content"),
            "error should mention content: {}",
            o.output
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    }

    #[test]
    fn write_file_explicit_empty_content_allowed() {
        let (_root, cfg) = tmp_ws();
        let path = cfg.workspace.join("wipe.txt");
        fs::write(&path, "old").unwrap();
        let o = execute(
            "write_file",
            &json!({"path": "wipe.txt", "content": ""}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
    }

    #[test]
    fn bulk_write_missing_content_errors_and_skips_file() {
        let (_root, cfg) = tmp_ws();
        let keep = cfg.workspace.join("keep.txt");
        fs::write(&keep, "safe").unwrap();
        let o = bulk_write(
            &json!({"files":[
                {"path":"keep.txt"},
                {"path":"ok.txt","content":"hi"}
            ]}),
            &cfg,
        );
        assert!(!o.ok, "{}", o.output);
        assert!(
            o.output.contains("content"),
            "error should mention content: {}",
            o.output
        );
        assert_eq!(fs::read_to_string(&keep).unwrap(), "safe");
        assert_eq!(
            fs::read_to_string(cfg.workspace.join("ok.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn bulk_write_primitive_no_longer_blocks_restricted_paths() {
        // Mirrors write_file: bulk_write calls write_file, which no longer
        // blocks. A restricted path (.env) is written at the primitive level;
        // the approval gate decides whether to prompt.
        let (_root, cfg) = tmp_ws();
        let o = bulk_write(
            &json!({"files":[{"path":".env","content":"LEAK=1"},{"path":"ok.txt","content":"hi"}]}),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(
            cfg.workspace.join(".env").exists(),
            ".env should be written (gate enforces, not the primitive)"
        );
        assert!(cfg.workspace.join("ok.txt").exists());
    }

    #[test]
    fn patch_blank_line_in_hunk_no_panic() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("p.txt"), "x\n").unwrap();
        // A hunk body with a blank context line previously panicked (P1-1):
        // the blank line advanced `target` past the end and the following `+`
        // line inserted at an out-of-bounds index.
        let diff = "@@ -1,1 +1,3 @@\n x\n\n+added\n";
        let o = execute("patch", &json!({"path":"p.txt","patch":diff}), &cfg);
        assert!(o.ok, "{}", o.output);
        let result = fs::read_to_string(cfg.workspace.join("p.txt")).unwrap();
        assert!(result.contains("added"), "{}", result);
    }

    #[tokio::test]
    async fn bash_denylist_blocks_extra_whitespace_root() {
        let (_root, cfg) = tmp_ws();
        // P1-7: extra spaces can't evade the pattern after whitespace normalization.
        let o = execute_bash("rm   -rf    /", &cfg, None, SudoAuth::None).await;
        assert!(!o.ok, "{}", o.output);
        assert!(o.output.contains("denylist"), "{}", o.output);
    }

    #[tokio::test]
    async fn bash_denylist_allows_tmp_subtree() {
        let (_root, cfg) = tmp_ws();
        // P1-7: `rm -rf /tmp/x` no longer false-positives on `rm -rf /`.
        // Use `echo` so nothing destructive runs; the tripwire must NOT match.
        let o = execute_bash("echo rm -rf /tmp/x-nope", &cfg, None, SudoAuth::None).await;
        assert!(o.ok, "{}", o.output);
        // And a plain workspace-relative rm still runs.
        fs::write(cfg.workspace.join("to_delete"), "x").unwrap();
        let o2 = execute_bash("rm -f to_delete", &cfg, None, SudoAuth::None).await;
        assert!(o2.ok, "{}", o2.output);
    }

    fn git_present() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Init a git repo in the workspace with a configured author + one commit.
    fn git_init(cfg: &Config) {
        let ws = &cfg.workspace;
        let _ = std::process::Command::new("git")
            .current_dir(ws)
            .args(["init"])
            .output();
        let _ = std::process::Command::new("git")
            .current_dir(ws)
            .args(["config", "user.email", "test@example.com"])
            .output();
        let _ = std::process::Command::new("git")
            .current_dir(ws)
            .args(["config", "user.name", "Test"])
            .output();
        std::fs::write(ws.join("README.md"), "hello\n").unwrap();
        let _ = std::process::Command::new("git")
            .current_dir(ws)
            .args(["add", "README.md"])
            .output();
        let _ = std::process::Command::new("git")
            .current_dir(ws)
            .args(["commit", "-m", "init"])
            .output();
    }

    #[test]
    fn unified_diff_insert_delete_modify() {
        let old = "a\nb\nc\n";
        let d = make_unified_diff(old, "a\nb2\nc\nd\n", "f.rs", 2);
        assert!(d.contains("--- a/f.rs"));
        assert!(d.contains("+++ b/f.rs"));
        assert!(d.contains("-b"), "diff: {d}");
        assert!(d.contains("+b2"), "diff: {d}");
        assert!(d.contains("+d"), "diff: {d}");
        // identical → empty
        assert_eq!(make_unified_diff(old, old, "f.rs", 3), "");
    }

    #[test]
    fn unified_diff_new_file_all_additions() {
        let d = make_unified_diff("", "x\ny\n", "new.rs", 3);
        assert!(d.contains("+x"), "diff: {d}");
        assert!(d.contains("+y"), "diff: {d}");
        // a brand-new file has no deletions
        assert!(!d.contains("\n-"), "diff should have no removed lines: {d}");
    }

    #[test]
    fn unified_diff_large_change_falls_back() {
        let big = "line\n".repeat(5000);
        // identical → short-circuits to empty before the size guard
        assert_eq!(make_unified_diff(&big, &big, "big.rs", 3), "");
        // a large *change* triggers the size-guard note (no OOM)
        let big2 = "other\n".repeat(5000);
        let d = make_unified_diff(&big, &big2, "big.rs", 3);
        assert!(d.contains("diff omitted for size"), "diff: {d}");
    }

    #[test]
    fn git_tools_roundtrip() {
        if !git_present() {
            eprintln!("skipping git tests: git not on PATH");
            return;
        }
        let (_root, cfg) = tmp_ws();
        git_init(&cfg);

        let s = execute("git_status", &json!({}), &cfg);
        assert!(s.ok, "git_status: {}", s.output);

        let l = execute("git_log", &json!({}), &cfg);
        assert!(l.ok, "git_log: {}", l.output);
        assert!(
            l.output.contains("init"),
            "git_log missing commit: {}",
            l.output
        );

        // modify → unstaged diff shows the change
        std::fs::write(cfg.workspace.join("README.md"), "hello world\n").unwrap();
        let d = execute("git_diff", &json!({}), &cfg);
        assert!(d.ok, "git_diff: {}", d.output);
        assert!(d.output.contains("-hello"), "git_diff: {}", d.output);

        // add + commit the change
        let a = execute("git_add", &json!({ "paths": ["README.md"] }), &cfg);
        assert!(a.ok, "git_add: {}", a.output);
        let c = execute("git_commit", &json!({ "message": "update readme" }), &cfg);
        assert!(c.ok, "git_commit: {}", c.output);

        // git_add rejects absolute / escaping paths
        let bad = execute("git_add", &json!({ "paths": ["/etc/hosts"] }), &cfg);
        assert!(!bad.ok, "git_add must reject absolute paths");
        let bad2 = execute("git_add", &json!({ "paths": ["../escape"] }), &cfg);
        assert!(!bad2.ok, "git_add must reject .. escapes");

        // git_commit rejects empty messages
        let bad3 = execute("git_commit", &json!({ "message": "   " }), &cfg);
        assert!(!bad3.ok, "git_commit must reject empty messages");
    }

    #[test]
    fn memory_tool_arg_validation() {
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "catalyst_memtool_root_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&root);
        let _mem_root = crate::memory::override_memory_root(root);
        let (_root, cfg) = tmp_ws();
        // unknown action
        let o = execute("memory", &json!({ "action": "nope" }), &cfg);
        assert!(!o.ok, "unknown action should fail");
        // save requires name + content (no disk write on validation failure)
        assert!(!execute("memory", &json!({ "action": "save", "content": "x" }), &cfg).ok);
        assert!(!execute("memory", &json!({ "action": "save", "name": "x" }), &cfg).ok);
        // forget requires id
        assert!(!execute("memory", &json!({ "action": "forget" }), &cfg).ok);
        // list is safe (read-only); tolerate empty store
        let l = execute("memory", &json!({ "action": "list" }), &cfg);
        assert!(l.ok, "list should always succeed: {}", l.output);
    }

    #[test]
    fn memory_tool_append_accumulates() {
        // append must accumulate onto a memory instead of overwriting it, so
        // repeated learnings about the same topic compound rather than clobber.
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "catalyst_memtool_root_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&root);
        let _mem_root = crate::memory::override_memory_root(root);
        let (_root, cfg) = tmp_ws();
        let save = execute(
            "memory",
            &json!({ "action": "save", "name": "conventions", "content": "use tabs", "type": "convention" }),
            &cfg,
        );
        assert!(save.ok, "save should succeed: {}", save.output);
        let ap = execute(
            "memory",
            &json!({ "action": "append", "name": "conventions", "content": "no unwrap in prod" }),
            &cfg,
        );
        assert!(ap.ok, "append should succeed: {}", ap.output);
        // Inspect the stored memory directly: list is catalog-only now.
        let entries = crate::memory::scan_memories(&cfg.workspace);
        assert_eq!(entries.len(), 1, "should be one accumulated memory");
        let c = &entries[0].content;
        assert!(c.contains("use tabs"), "original fact must survive: {c}");
        assert!(
            c.contains("no unwrap in prod"),
            "appended fact must be present: {c}"
        );
        assert!(
            c.contains("--- appended ---"),
            "append marker must be present: {c}"
        );
        // append validates the same way save does
        assert!(
            !execute(
                "memory",
                &json!({ "action": "append", "content": "x" }),
                &cfg
            )
            .ok
        );
        assert!(!execute("memory", &json!({ "action": "append", "name": "x" }), &cfg).ok);
    }

    #[test]
    fn memory_tool_save_redirects_to_append_when_name_exists() {
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "catalyst_memtool_root_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&root);
        let _mem_root = crate::memory::override_memory_root(root);
        let (_root, cfg) = tmp_ws();
        assert!(execute(
            "memory",
            &json!({ "action": "save", "name": "topic", "content": "fact one is durable here", "description": "t" }),
            &cfg,
        )
        .ok);
        let o = execute(
            "memory",
            &json!({ "action": "save", "name": "topic", "content": "fact two is also durable", "description": "t" }),
            &cfg,
        );
        assert!(o.ok, "{}", o.output);
        assert!(
            o.output.contains("appended") || o.output.contains("name exists"),
            "second save should append: {}",
            o.output
        );
        let entries = crate::memory::scan_memories(&cfg.workspace);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("fact one"));
        assert!(entries[0].content.contains("fact two"));
    }

    #[test]
    fn memory_tool_get_returns_full_body() {
        let _serial = crate::memory::memory_test_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!(
            "catalyst_memtool_root_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&root);
        let _mem_root = crate::memory::override_memory_root(root);
        let (_root, cfg) = tmp_ws();
        assert!(
            execute(
                "memory",
                &json!({
                    "action": "save",
                    "name": "deep",
                    "content": "line1\nline2\nline3\nline4",
                    "description": "deep note",
                    "type": "note"
                }),
                &cfg,
            )
            .ok
        );
        let list = execute("memory", &json!({ "action": "list" }), &cfg);
        assert!(list.ok, "{}", list.output);
        assert!(list.output.contains("deep"));
        assert!(
            !list.output.contains("line4"),
            "list must stay catalog-only: {}",
            list.output
        );
        let got = execute("memory", &json!({ "action": "get", "id": "deep" }), &cfg);
        assert!(got.ok, "{}", got.output);
        assert!(got.output.contains("line4"), "get must return full body");
        assert!(!execute("memory", &json!({ "action": "get" }), &cfg).ok);
    }

    #[test]
    fn workspace_activity_lists_peers() {
        let (_root, cfg) = tmp_ws();
        let my_pid = std::process::id();

        // No peers → reassuring "you're alone" message.
        let o = execute("workspace_activity", &json!({}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("No other active"), "{}", o.output);

        // Seed a fake peer (a different pid) in this workspace's presence dir.
        let peer_pid = my_pid.wrapping_add(1);
        let peer = crate::presence::PresenceRecord::from_work_state(
            &crate::WorkState {
                goal: "fix CI".into(),
                recent_files: vec!["core/src/main.rs".into()],
                in_progress: vec!["green build".into()],
                ..Default::default()
            },
            peer_pid,
            Some("peer.json".into()),
            None,
            crate::presence::unix_now(),
        );
        crate::presence::write_presence(&cfg.workspace, peer_pid, &peer);

        let o = execute("workspace_activity", &json!({}), &cfg);
        assert!(o.ok, "{}", o.output);
        assert!(o.output.contains("1 other active session"), "{}", o.output);
        assert!(o.output.contains("fix CI"), "goal missing: {}", o.output);
        assert!(
            o.output.contains("core/src/main.rs"),
            "recent file missing: {}",
            o.output
        );
        assert!(
            o.output.contains("green build"),
            "in-progress missing: {}",
            o.output
        );
        assert!(
            o.output.contains(&format!("pid {peer_pid}")),
            "pid missing: {}",
            o.output
        );

        // Self (my_pid) must never appear even if our own presence file exists.
        let me = crate::presence::PresenceRecord::from_work_state(
            &crate::WorkState::default(),
            my_pid,
            None,
            None,
            crate::presence::unix_now(),
        );
        crate::presence::write_presence(&cfg.workspace, my_pid, &me);
        let o = execute("workspace_activity", &json!({}), &cfg);
        assert!(
            !o.output.contains(&format!("pid {my_pid}\n")),
            "self leaked: {}",
            o.output
        );

        crate::presence::clear_presence(&cfg.workspace, peer_pid);
        crate::presence::clear_presence(&cfg.workspace, my_pid);
    }

    #[test]
    fn test_command_uses_sudo_detection() {
        // Positive: commands that invoke sudo as a command word.
        assert!(command_uses_sudo("sudo apt update"));
        assert!(command_uses_sudo("sudo make install"));
        assert!(command_uses_sudo("cd /opt && sudo ./install.sh"));
        assert!(command_uses_sudo("echo hi | sudo tee /etc/hosts"));
        assert!(command_uses_sudo("sudo -u root whoami"));

        // Negative: sudo as a substring but NOT a standalone word.
        assert!(!command_uses_sudo("sudoers"));
        assert!(!command_uses_sudo("pseudo command"));
        assert!(!command_uses_sudo("ls -la"));
        assert!(!command_uses_sudo("echo hello"));
        assert!(!command_uses_sudo(""));
    }

    #[test]
    fn sudo_preflight_only_identifies_password_diagnostics() {
        assert_eq!(
            classify_sudo_preflight(true, b""),
            SudoPreflight::NonInteractive
        );
        assert_eq!(
            classify_sudo_preflight(false, b"sudo: a password is required\n"),
            SudoPreflight::PasswordRequired
        );
        assert_eq!(
            classify_sudo_preflight(false, b"sudo: user is not allowed to execute true\n"),
            SudoPreflight::Unavailable
        );
        assert_eq!(
            classify_sudo_preflight(false, b"sudo: command not found\n"),
            SudoPreflight::Unavailable
        );
    }

    #[test]
    fn sudo_prompt_respects_permission_mode_and_password_state() {
        assert!(!sudo_should_prompt(
            &Approval::Never,
            SudoPreflight::NonInteractive
        ));
        assert!(!sudo_should_prompt(
            &Approval::Never,
            SudoPreflight::Unavailable
        ));
        assert!(sudo_should_prompt(
            &Approval::Never,
            SudoPreflight::PasswordRequired
        ));

        for approval in [Approval::Destructive, Approval::Always] {
            assert!(sudo_should_prompt(&approval, SudoPreflight::NonInteractive));
            assert!(sudo_should_prompt(&approval, SudoPreflight::Unavailable));
        }
    }

    #[test]
    fn shell_resolution_default_is_posix_on_unix() {
        // On a POSIX host with no CATALYST_CODE_SHELL override, the shell is
        // bash and shell_argv produces the `bash -c <cmd>` form. (This test
        // does NOT mutate any env var, so it is safe under `cargo test`'s
        // parallel runner — it only reads the default.) The PowerShell branch
        // is compile-gated to Windows and covered by the cross-compile check.
        if cfg!(target_os = "windows") {
            return; // default is PowerShell here; skip on a Windows host.
        }
        assert!(shell_is_posix(), "default Unix shell should be POSIX");
        let (prog, args) = shell_argv("echo hi");
        assert_eq!(prog, "bash");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
        // A bash sudo command is still detected; a fake PowerShell-only host
        // can't run here, but command_uses_sudo must agree with shell_is_posix.
        assert!(command_uses_sudo("sudo true"));
    }

    #[tokio::test]
    async fn test_bash_sudo_without_password_returns_error() {
        let (_root, cfg) = tmp_ws();
        // A sudo command with no password (un-approved path, e.g. subagent/bulk)
        // must return a clean error — NOT run sudo (which would grab /dev/tty
        // and garble the TUI).
        let o = execute_bash("sudo true", &cfg, None, SudoAuth::None).await;
        assert!(!o.ok, "sudo without password should not succeed");
        assert!(
            o.output.contains("sudo"),
            "error should mention sudo: {}",
            o.output
        );
    }

    #[tokio::test]
    async fn unified_read_handles_workspace_and_rejects_uri_escapes() {
        let (_root, cfg) = tmp_ws();
        fs::write(cfg.workspace.join("note.txt"), "safe\n").unwrap();
        let file = execute_unified_read(&json!({"path":"note.txt"}), &cfg, None).await;
        assert!(file.ok, "{}", file.output);
        assert_eq!(file.output, "safe\n");
        for path in [
            "../escape",
            "memory://../secret",
            "skill://../secret",
            "artifact://../secret",
            "archive.zip!/../secret",
        ] {
            let outcome = execute_unified_read(&json!({"path": path}), &cfg, None).await;
            assert!(
                !outcome.ok,
                "{path} unexpectedly succeeded: {}",
                outcome.output
            );
        }
    }
}
