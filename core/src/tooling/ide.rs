//! Deferred IDE tooling: bounded LSP JSON-RPC, snapshot-tagged edits, and ast-grep.
use crate::config::Config;
use crate::tools::Outcome;
use ast_grep_language::{LanguageExt, SupportLang};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const MAX_RPC_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_AST_EDIT_BYTES: usize = 2 * 1024 * 1024;
const LSP_TIMEOUT: Duration = Duration::from_secs(20);

fn path(cfg: &Config, raw: &str) -> Result<PathBuf, String> {
    crate::workspace::resolve(&cfg.workspace, raw)
}
fn tag(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{:x}", h.finalize())
}
fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        s.truncate(MAX_OUTPUT);
        s.push_str("\n[output truncated]");
    }
    s
}

/// Apply line edits only when every edit carries the current whole-file snapshot tag.
pub fn execute_snapshot_edit(args: &Value, cfg: &Config) -> Outcome {
    let raw = args.get("path").and_then(Value::as_str).unwrap_or("");
    let p = match path(cfg, raw) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let old = match std::fs::read(&p) {
        Ok(v) => v,
        Err(e) => return Outcome::err(format!("snapshot_edit read failed: {e}")),
    };
    let current = tag(&old);
    let edits = match args.get("edits").and_then(Value::as_array) {
        Some(v) if !v.is_empty() => v,
        _ => return Outcome::err("snapshot_edit requires non-empty edits"),
    };
    let mut lines: Vec<String> = String::from_utf8_lossy(&old)
        .split_inclusive('\n')
        .map(str::to_string)
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let mut ops = Vec::new();
    for e in edits {
        let got = e
            .get("tag")
            .or_else(|| e.get("expected_tag"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if got != current {
            return Outcome::err(format!("stale snapshot tag: expected {current}, got {got}"));
        }
        let start = e.get("start_line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let end = e
            .get("end_line")
            .and_then(Value::as_u64)
            .unwrap_or(start as u64) as usize;
        if start == 0 || end < start || end > lines.len() {
            return Outcome::err("snapshot_edit line range is out of bounds");
        }
        let replacement = match e.get("replacement").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return Outcome::err("snapshot_edit replacement must be a string"),
        };
        ops.push((start - 1, end, replacement));
    }
    ops.sort_by_key(|(s, _, _)| *s);
    for w in ops.windows(2) {
        if w[0].1 > w[1].0 {
            return Outcome::err("snapshot_edit ranges overlap");
        }
    }
    for (s, e, r) in ops.into_iter().rev() {
        lines.splice(s..e, r.split_inclusive('\n').map(str::to_string));
    }
    let new = lines.concat();
    if let Err(e) = crate::fsutil::atomic_write_str(&p, &new) {
        return Outcome::err(format!("snapshot_edit write failed: {e}"));
    }
    Outcome::ok(format!(
        "applied snapshot_edit to {raw}; new_tag={}",
        tag(new.as_bytes())
    ))
}

async fn rpc_write(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if body.len() > MAX_RPC_BYTES {
        return Err("LSP request exceeds size limit".into());
    }
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stdin.write_all(&body).await.map_err(|e| e.to_string())?;
    stdin.flush().await.map_err(|e| e.to_string())
}
async fn rpc_read(stdout: &mut tokio::process::ChildStdout) -> Result<Value, String> {
    let mut headers = Vec::new();
    let mut one = [0u8; 1];
    loop {
        stdout
            .read_exact(&mut one)
            .await
            .map_err(|e| e.to_string())?;
        headers.push(one[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 4096 {
            return Err("LSP headers too large".into());
        }
    }
    let hs = String::from_utf8_lossy(&headers);
    let len = hs
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length:")
                .and_then(|n| n.trim().parse::<usize>().ok())
        })
        .ok_or("missing LSP Content-Length")?;
    if len > MAX_RPC_BYTES {
        return Err("LSP response exceeds size limit".into());
    }
    let mut body = vec![0; len];
    stdout
        .read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| format!("invalid LSP JSON: {e}"))
}

/// Language servers the model may spawn. Free-form `command` is otherwise an
/// arbitrary host process under ReadOnly+Never approval (CORE_REVIEW C4).
const LSP_COMMAND_ALLOWLIST: &[&str] = &[
    "rust-analyzer",
    "gopls",
    "typescript-language-server",
    "pyright",
    "pyright-langserver",
    "pylsp",
    "clangd",
    "lua-language-server",
    "zls",
    "jdtls",
    "kotlin-language-server",
    "bash-language-server",
    "yaml-language-server",
    "vscode-json-language-server",
    "vscode-css-language-server",
    "vscode-html-language-server",
    "texlab",
    "haskell-language-server",
    "hls",
    "solargraph",
    "ruby-lsp",
    "intelephense",
    "phpactor",
    "omnisharp",
    "csharp-ls",
    "ruff",
    "ruff-lsp",
    "deno",
    "biome",
    "eslint-lsp",
    "tailwindcss-language-server",
];

fn validate_lsp_command(command: &str) -> Result<(), String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err("lsp command must not be empty".into());
    }
    // Reject path separators / absolute paths — allowlist is bare names only.
    if cmd.contains('/') || cmd.contains('\\') || cmd.contains("..") {
        return Err(format!(
            "lsp command must be a bare allowlisted server name, not a path ({cmd})"
        ));
    }
    // Reject shell metacharacters.
    if cmd.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '`' | '$' | '(' | ')' | '<' | '>' | '\n' | '\r' | ' '
        )
    }) {
        return Err(format!("lsp command contains disallowed characters: {cmd}"));
    }
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    if !LSP_COMMAND_ALLOWLIST.iter().any(|a| *a == base) {
        return Err(format!(
            "lsp command '{cmd}' is not allowlisted. Allowed: {}",
            LSP_COMMAND_ALLOWLIST.join(", ")
        ));
    }
    Ok(())
}

/// Execute a single bounded LSP request. The server is always killed and waited on.
pub async fn execute_lsp(args: &Value, cfg: &Config) -> Outcome {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("diagnostics");
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("rust-analyzer");
    if let Err(e) = validate_lsp_command(command) {
        return Outcome::err(e);
    }
    let file = args.get("path").and_then(Value::as_str).unwrap_or("");
    let p = match path(cfg, file) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let uri = format!("file://{}", p.display());
    let (method, params) = match action {
        "diagnostics" => (
            "textDocument/diagnostic",
            json!({"textDocument":{"uri":uri}}),
        ),
        "definition" => (
            "textDocument/definition",
            json!({"textDocument":{"uri":uri},"position":{"line":args.get("line").and_then(Value::as_u64).unwrap_or(0),"character":args.get("character").and_then(Value::as_u64).unwrap_or(0)}}),
        ),
        "references" => (
            "textDocument/references",
            json!({"textDocument":{"uri":uri},"position":{"line":args.get("line").and_then(Value::as_u64).unwrap_or(0),"character":args.get("character").and_then(Value::as_u64).unwrap_or(0)},"context":{"includeDeclaration":true}}),
        ),
        "rename" => (
            "textDocument/rename",
            json!({"textDocument":{"uri":uri},"position":{"line":args.get("line").and_then(Value::as_u64).unwrap_or(0),"character":args.get("character").and_then(Value::as_u64).unwrap_or(0)},"newName":args.get("new_name").and_then(Value::as_str).unwrap_or("")}),
        ),
        _ => return Outcome::err("unsupported LSP action"),
    };
    let mut child = match Command::new(command)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("LSP server unavailable: {e}")),
    };
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let result = tokio::time::timeout(LSP_TIMEOUT, async {
        rpc_write(&mut input, &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"rootUri":format!("file://{}",cfg.workspace.display()),"capabilities":{}}})).await?;
        let _ = rpc_read(&mut output).await?;
        rpc_write(&mut input, &json!({"jsonrpc":"2.0","method":"initialized","params":{}})).await?;
        rpc_write(&mut input, &json!({"jsonrpc":"2.0","id":2,"method":method,"params":params})).await?;
        rpc_read(&mut output).await
    }).await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    match result {
        Ok(Ok(v)) => Outcome::ok(truncate(v.to_string())),
        Ok(Err(e)) => Outcome::err(e),
        Err(_) => Outcome::err("LSP request timed out"),
    }
}

/// Return the embedded parser for a release-supported source file. This is
/// intentionally extension-driven: callers cannot choose an arbitrary parser or
/// executable, and unsupported languages fail explicitly instead of becoming a
/// textual search-and-replace.
fn ast_language(path: &Path) -> Result<SupportLang, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Ok(SupportLang::Rust),
        "go" => Ok(SupportLang::Go),
        "js" | "mjs" | "cjs" => Ok(SupportLang::JavaScript),
        "ts" | "mts" | "cts" => Ok(SupportLang::TypeScript),
        "tsx" | "jsx" => Ok(SupportLang::Tsx),
        "py" | "pyi" => Ok(SupportLang::Python),
        "json" | "jsonc" => Ok(SupportLang::Json),
        "yaml" | "yml" => Ok(SupportLang::Yaml),
        "md" | "mdx" => Ok(SupportLang::Markdown),
        _ => Err(format!(
            "ast_edit does not embed a parser for '.{extension}'; supported extensions: rs, go, js, ts, tsx, py, json, yaml, md"
        )),
    }
}

/// Execute an embedded ast-grep structural rewrite. `apply:false` returns the
/// staged diff; `apply:true` atomically updates the workspace-confined file.
pub async fn execute_ast_edit(args: &Value, cfg: &Config) -> Outcome {
    if args.get("binary").is_some() {
        return Outcome::err("ast_edit uses the embedded parser; 'binary' is no longer supported");
    }
    let raw_path = args.get("path").and_then(Value::as_str).unwrap_or("");
    let p = match path(cfg, raw_path) {
        Ok(p) => p,
        Err(e) => return Outcome::err(e),
    };
    let language = match ast_language(&p) {
        Ok(language) => language,
        Err(error) => return Outcome::err(error),
    };
    let source = match std::fs::read_to_string(&p) {
        Ok(source) => source,
        Err(error) => return Outcome::err(format!("ast_edit read failed: {error}")),
    };
    if source.len() > MAX_AST_EDIT_BYTES {
        return Outcome::err(format!(
            "ast_edit file exceeds the {MAX_AST_EDIT_BYTES}-byte embedded parser limit"
        ));
    }
    let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
    let rewrite = args.get("rewrite").and_then(Value::as_str).unwrap_or("");
    if pattern.is_empty() || rewrite.is_empty() {
        return Outcome::err("ast_edit requires non-empty pattern and rewrite");
    }

    let root = language.ast_grep(&source);
    let edits = root.root().replace_all(pattern, rewrite);
    if edits.is_empty() {
        return Outcome::ok(format!("ast_edit found no matches in {raw_path}"));
    }
    let mut rewritten = source.clone();
    for edit in edits.iter().rev() {
        let end = edit.position.saturating_add(edit.deleted_length);
        if end > rewritten.len()
            || !rewritten.is_char_boundary(edit.position)
            || !rewritten.is_char_boundary(end)
        {
            return Outcome::err("ast_edit generated an invalid UTF-8 edit range");
        }
        let replacement = match String::from_utf8(edit.inserted_text.clone()) {
            Ok(replacement) => replacement,
            Err(_) => return Outcome::err("ast_edit generated a non-UTF-8 replacement"),
        };
        rewritten.replace_range(edit.position..end, &replacement);
    }
    let diff = crate::tools::make_unified_diff(&source, &rewritten, raw_path, 3);
    let apply = args.get("apply").and_then(Value::as_bool).unwrap_or(false);
    if apply {
        if let Err(error) = crate::fsutil::atomic_write_str(&p, &rewritten) {
            return Outcome::err(format!("ast_edit write failed: {error}"));
        }
    }
    let state = if apply { "applied" } else { "staged" };
    let mut outcome = Outcome::ok(format!(
        "{state} {} structural rewrite(s) in {raw_path}",
        edits.len()
    ));
    outcome.diff = Some(truncate(diff));
    outcome
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_tag_rejected_without_write() {
        let d = std::env::temp_dir().join(format!("catcode-ide-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("x");
        std::fs::write(&p, "a\nb\n").unwrap();
        let mut c = Config::default();
        c.workspace = d.clone();
        let o = execute_snapshot_edit(
            &json!({"path":"x","edits":[{"start_line":1,"end_line":1,"tag":"sha256:bad","replacement":"z"}]}),
            &c,
        );
        assert!(!o.ok);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n");
        let _ = std::fs::remove_dir_all(d);
    }

    fn ast_config(dir: &Path) -> Config {
        Config {
            workspace: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn embedded_ast_edit_stages_then_applies_rust_rewrite() {
        let d = std::env::temp_dir().join(format!("catcode-ast-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("lib.rs");
        std::fs::write(&p, "fn main() { old(); old(); }\n").unwrap();
        let cfg = ast_config(&d);
        let args = json!({"path":"lib.rs","pattern":"old()","rewrite":"new()"});

        let staged = execute_ast_edit(&args, &cfg).await;
        assert!(staged.ok, "{}", staged.output);
        assert!(staged.output.contains("staged 2 structural rewrite"));
        let staged_diff = staged.diff.as_deref().unwrap_or_default();
        assert!(
            staged_diff.contains("new"),
            "staged diff missing rewrite: {staged_diff}"
        );
        assert!(
            staged_diff.contains("-"),
            "staged path must return a unified diff"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn main() { old(); old(); }\n",
            "apply:false must not mutate the file"
        );

        let applied = execute_ast_edit(
            &json!({"path":"lib.rs","pattern":"old()","rewrite":"new()","apply":true}),
            &cfg,
        )
        .await;
        assert!(applied.ok, "{}", applied.output);
        assert!(applied.output.contains("applied 2 structural rewrite"));
        assert!(
            applied.diff.as_deref().unwrap_or_default().contains("new"),
            "apply:true must still return the rewrite diff"
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "fn main() { new(); new(); }\n"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test]
    async fn embedded_ast_edit_rejects_unsupported_language_and_binary_override() {
        let d =
            std::env::temp_dir().join(format!("catcode-ast-edit-invalid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("data.toml"), "key = 'old'\n").unwrap();
        std::fs::write(d.join("main.rs"), "fn old() {}\n").unwrap();
        let cfg = ast_config(&d);
        let unsupported = execute_ast_edit(
            &json!({"path":"data.toml","pattern":"old","rewrite":"new"}),
            &cfg,
        )
        .await;
        assert!(!unsupported.ok);
        assert!(unsupported.output.contains("does not embed a parser"));
        let override_attempt = execute_ast_edit(
            &json!({"path":"main.rs","pattern":"old()","rewrite":"new()","binary":"sg"}),
            &cfg,
        )
        .await;
        assert!(!override_attempt.ok);
        assert!(override_attempt.output.contains("binary"));
        let empty = execute_ast_edit(
            &json!({"path":"main.rs","pattern":"","rewrite":"new()"}),
            &cfg,
        )
        .await;
        assert!(!empty.ok);
        assert!(empty.output.contains("non-empty pattern and rewrite"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test]
    async fn embedded_ast_edit_covers_go_python_ts_json_and_no_match() {
        let d = std::env::temp_dir().join(format!(
            "catcode-ast-edit-langs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("main.go"),
            "package main\n\nfunc main() { oldName(1) }\n",
        )
        .unwrap();
        std::fs::write(d.join("app.py"), "def main():\n    old_name(1)\n").unwrap();
        std::fs::write(d.join("app.ts"), "function main() { oldName(1); }\n").unwrap();
        std::fs::write(d.join("cfg.json"), r#"{"service":"legacy"}"#).unwrap();
        std::fs::write(d.join("lib.rs"), "fn main() { keep(); }\n").unwrap();
        let cfg = ast_config(&d);

        let go = execute_ast_edit(
            &json!({"path":"main.go","pattern":"oldName($A)","rewrite":"newName($A)","apply":true}),
            &cfg,
        )
        .await;
        assert!(go.ok, "{}", go.output);
        assert!(std::fs::read_to_string(d.join("main.go"))
            .unwrap()
            .contains("newName(1)"));

        let py = execute_ast_edit(
            &json!({"path":"app.py","pattern":"old_name($A)","rewrite":"new_name($A)","apply":true}),
            &cfg,
        )
        .await;
        assert!(py.ok, "{}", py.output);
        assert!(std::fs::read_to_string(d.join("app.py"))
            .unwrap()
            .contains("new_name(1)"));

        let ts = execute_ast_edit(
            &json!({"path":"app.ts","pattern":"oldName($A)","rewrite":"newName($A)","apply":true}),
            &cfg,
        )
        .await;
        assert!(ts.ok, "{}", ts.output);
        assert!(std::fs::read_to_string(d.join("app.ts"))
            .unwrap()
            .contains("newName(1)"));

        let json_out = execute_ast_edit(
            &json!({"path":"cfg.json","pattern":"\"legacy\"","rewrite":"\"current\"","apply":true}),
            &cfg,
        )
        .await;
        assert!(json_out.ok, "{}", json_out.output);
        assert!(std::fs::read_to_string(d.join("cfg.json"))
            .unwrap()
            .contains("current"));

        let none = execute_ast_edit(
            &json!({"path":"lib.rs","pattern":"missing($X)","rewrite":"x($X)"}),
            &cfg,
        )
        .await;
        assert!(none.ok, "{}", none.output);
        assert!(none.output.contains("found no matches"));
        assert_eq!(
            std::fs::read_to_string(d.join("lib.rs")).unwrap(),
            "fn main() { keep(); }\n"
        );
        assert!(none.diff.is_none());

        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn ast_language_maps_supported_extensions() {
        assert!(matches!(
            ast_language(Path::new("a.rs")),
            Ok(SupportLang::Rust)
        ));
        assert!(matches!(
            ast_language(Path::new("a.go")),
            Ok(SupportLang::Go)
        ));
        assert!(matches!(
            ast_language(Path::new("a.js")),
            Ok(SupportLang::JavaScript)
        ));
        assert!(matches!(
            ast_language(Path::new("a.mjs")),
            Ok(SupportLang::JavaScript)
        ));
        assert!(matches!(
            ast_language(Path::new("a.ts")),
            Ok(SupportLang::TypeScript)
        ));
        assert!(matches!(
            ast_language(Path::new("a.tsx")),
            Ok(SupportLang::Tsx)
        ));
        assert!(matches!(
            ast_language(Path::new("a.jsx")),
            Ok(SupportLang::Tsx)
        ));
        assert!(matches!(
            ast_language(Path::new("a.py")),
            Ok(SupportLang::Python)
        ));
        assert!(matches!(
            ast_language(Path::new("a.json")),
            Ok(SupportLang::Json)
        ));
        assert!(matches!(
            ast_language(Path::new("a.yaml")),
            Ok(SupportLang::Yaml)
        ));
        assert!(matches!(
            ast_language(Path::new("a.yml")),
            Ok(SupportLang::Yaml)
        ));
        assert!(matches!(
            ast_language(Path::new("a.md")),
            Ok(SupportLang::Markdown)
        ));
        assert!(ast_language(Path::new("a.toml")).is_err());
        assert!(ast_language(Path::new("a.c")).is_err());
    }

    #[tokio::test]
    async fn embedded_ast_edit_rejects_workspace_escape_and_missing_file() {
        let d = std::env::temp_dir().join(format!(
            "catcode-ast-edit-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("lib.rs"), "fn main() { old(); }\n").unwrap();
        let cfg = ast_config(&d);

        let escape = execute_ast_edit(
            &json!({
                "path": "../outside.rs",
                "pattern": "old()",
                "rewrite": "new()",
                "apply": true
            }),
            &cfg,
        )
        .await;
        assert!(!escape.ok, "workspace escape must fail: {}", escape.output);
        assert!(
            escape.output.contains("..") || escape.output.contains("outside"),
            "unexpected escape error: {}",
            escape.output
        );
        // Ensure a successful product rewrite path still exists so this test
        // fails if execute_ast_edit is bypassed or broken while raw ast-grep works.
        assert_eq!(
            std::fs::read_to_string(d.join("lib.rs")).unwrap(),
            "fn main() { old(); }\n"
        );

        let missing = execute_ast_edit(
            &json!({"path":"missing.rs","pattern":"old()","rewrite":"new()"}),
            &cfg,
        )
        .await;
        assert!(!missing.ok);
        assert!(missing.output.contains("ast_edit read failed"));

        let _ = std::fs::remove_dir_all(d);
    }

    #[tokio::test]
    async fn embedded_ast_edit_yaml_and_markdown_apply() {
        let d = std::env::temp_dir().join(format!(
            "catcode-ast-edit-yml-md-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("cfg.yaml"), "service: legacy\nport: 8080\n").unwrap();
        std::fs::write(d.join("doc.md"), "# Title\n\nCall `old_fn` please.\n").unwrap();
        let cfg = ast_config(&d);

        let yml = execute_ast_edit(
            &json!({
                "path": "cfg.yaml",
                "pattern": "legacy",
                "rewrite": "current",
                "apply": true
            }),
            &cfg,
        )
        .await;
        assert!(yml.ok, "{}", yml.output);
        assert!(std::fs::read_to_string(d.join("cfg.yaml"))
            .unwrap()
            .contains("current"));

        let md = execute_ast_edit(
            &json!({
                "path": "doc.md",
                "pattern": "old_fn",
                "rewrite": "new_fn",
                "apply": true
            }),
            &cfg,
        )
        .await;
        assert!(md.ok, "{}", md.output);
        assert!(std::fs::read_to_string(d.join("doc.md"))
            .unwrap()
            .contains("new_fn"));

        let _ = std::fs::remove_dir_all(d);
    }
}
