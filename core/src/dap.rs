//! Deferred Debug Adapter Protocol tool.
//!
//! One adapter process is retained per Catalyst session. Transport is the DAP
//! `Content-Length` protocol over stdio; all buffers and waits are bounded.

use crate::runtime::{ResourceKind, ResourceLease};
use crate::tooling::ToolExecutionContext;
use crate::tools::Outcome;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_EVENTS: usize = 128;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

static SESSIONS: OnceLock<Mutex<HashMap<String, Arc<DapSession>>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, Arc<DapSession>>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct DapSession {
    stdin: AsyncMutex<ChildStdin>,
    child: AsyncMutex<Child>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    events: Mutex<Vec<Value>>,
    stderr: Mutex<String>,
    sequence: std::sync::atomic::AtomicU64,
    dead: CancellationToken,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr_reader: Mutex<Option<JoinHandle<()>>>,
    _resource: Option<ResourceLease>,
}

impl DapSession {
    async fn request(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Value, String> {
        if self.dead.is_cancelled() {
            return Err(self.death_error());
        }
        let seq = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let message = json!({
            "seq": seq,
            "type": "request",
            "command": command,
            "arguments": arguments
        });
        let body = serde_json::to_vec(&message)
            .map_err(|e| format!("DAP request encoding failed: {e}"))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(format!("DAP request exceeds {MAX_FRAME_BYTES} byte limit"));
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(seq, tx);
        let write_result = async {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
                .await?;
            stdin.write_all(&body).await?;
            stdin.flush().await
        }
        .await;
        if let Err(e) = write_result {
            lock(&self.pending).remove(&seq);
            return Err(format!("DAP adapter write failed: {e}"));
        }
        let result = tokio::select! {
            _ = cancellation.cancelled() => Err("DAP request cancelled".to_string()),
            _ = self.dead.cancelled() => Err(self.death_error()),
            result = tokio::time::timeout(timeout, rx) => match result {
                Err(_) => Err(format!("DAP {command} timed out after {}ms", timeout.as_millis())),
                Ok(Err(_)) => Err(self.death_error()),
                Ok(Ok(result)) => result,
            },
        };
        lock(&self.pending).remove(&seq);
        result
    }

    fn death_error(&self) -> String {
        let stderr = lock(&self.stderr);
        if stderr.is_empty() {
            "DAP adapter exited or closed its protocol stream".into()
        } else {
            format!("DAP adapter exited: {}", stderr.trim())
        }
    }

    fn take_events(&self) -> Vec<Value> {
        std::mem::take(&mut *lock(&self.events))
    }

    async fn stop(&self) {
        self.dead.cancel();
        let _ = self.child.lock().await.kill().await;
        if let Some(task) = lock(&self.reader).take() {
            task.abort();
        }
        if let Some(task) = lock(&self.stderr_reader).take() {
            task.abort();
        }
        fail_pending(&self.pending, "DAP session disconnected");
    }
}

/// Dispatch the single deferred `debug` tool.
pub async fn dispatch(args: &Value, ctx: &ToolExecutionContext) -> Outcome {
    match dispatch_inner(args, ctx).await {
        Ok(value) => Outcome::ok(value.to_string()),
        Err(error) => Outcome::err(error),
    }
}

async fn dispatch_inner(args: &Value, ctx: &ToolExecutionContext) -> Result<Value, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .ok_or("debug requires string 'action'")?;
    let session_id = ctx.session_id().to_string();
    if action == "initialize" {
        let old = lock(sessions()).remove(&session_id);
        if let Some(old) = old {
            old.stop().await;
        }
        let session = spawn_adapter(args, ctx).await?;
        lock(sessions()).insert(session_id, session.clone());
        let initialize_args = args.get("arguments").cloned().unwrap_or_else(|| {
            json!({
                "clientID": "catalyst-code",
                "clientName": "Catalyst Code",
                "pathFormat": "path",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false
            })
        });
        let response = session
            .request(
                "initialize",
                initialize_args,
                request_timeout(args),
                ctx.cancellation(),
            )
            .await;
        if response.is_err() {
            lock(sessions()).remove(ctx.session_id());
            session.stop().await;
        }
        return response.map(|response| result_with_events(response, &session));
    }
    let session = lock(sessions())
        .get(&session_id)
        .cloned()
        .ok_or("no DAP session; call debug with action='initialize' and adapter command first")?;
    if action == "output" {
        return Ok(json!({"events": session.take_events()}));
    }
    if action == "disconnect" {
        let response = session
            .request(
                "disconnect",
                arguments(args),
                request_timeout(args),
                ctx.cancellation(),
            )
            .await;
        lock(sessions()).remove(&session_id);
        session.stop().await;
        return response.map(|response| result_with_events(response, &session));
    }
    validate_debug_arguments(action, args.get("arguments"), ctx.workspace())?;
    // write_memory / evaluate can mutate target process memory or run
    // expressions with full debuggee privileges. Schema claims extra approval;
    // require an explicit confirm flag so a single Destructive approve of
    // `debug` cannot silently escalate (CORE_REVIEW C7).
    if matches!(action, "write_memory" | "evaluate") {
        let confirmed = args
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
            || args
                .get("arguments")
                .and_then(|v| v.get("confirm"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        if !confirmed {
            return Err(format!(
                "debug action '{action}' requires confirm:true (memory write / evaluate can                  alter or inspect the debuggee arbitrarily)"
            ));
        }
    }
    let command = match action {
        "launch" => "launch",
        "attach" => "attach",
        "set_breakpoints" => "setBreakpoints",
        "continue" => "continue",
        "next" | "step_over" => "next",
        "step_in" => "stepIn",
        "step_out" => "stepOut",
        "pause" => "pause",
        "threads" => "threads",
        "stack_trace" => "stackTrace",
        "scopes" => "scopes",
        "variables" => "variables",
        "evaluate" => "evaluate",
        "read_memory" => "readMemory",
        "write_memory" => "writeMemory",
        other => return Err(format!("unknown debug action '{other}'")),
    };
    let response = session
        .request(
            command,
            arguments(args),
            request_timeout(args),
            ctx.cancellation(),
        )
        .await?;
    Ok(result_with_events(response, &session))
}

fn arguments(args: &Value) -> Value {
    args.get("arguments").cloned().unwrap_or_else(|| json!({}))
}

fn result_with_events(response: Value, session: &DapSession) -> Value {
    json!({"response": response, "events": session.take_events()})
}

fn request_timeout(args: &Value) -> Duration {
    Duration::from_millis(
        args.get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS),
    )
}

async fn spawn_adapter(
    args: &Value,
    ctx: &ToolExecutionContext,
) -> Result<Arc<DapSession>, String> {
    let workspace = ctx.workspace();
    let adapter = args
        .get("adapter")
        .and_then(Value::as_object)
        .ok_or("debug initialize requires adapter object with command and optional args")?;
    let command = adapter
        .get("command")
        .and_then(Value::as_str)
        .ok_or("adapter.command must be a string")?;
    let executable = validate_command(command, workspace)?;
    let cwd = match adapter.get("cwd").and_then(Value::as_str) {
        Some(path) => resolve_workspace_path(workspace, path)?,
        None => workspace.to_path_buf(),
    };
    let mut process = Command::new(executable);
    process
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(values) = adapter.get("args").and_then(Value::as_array) {
        for value in values {
            process.arg(
                value
                    .as_str()
                    .ok_or("adapter.args entries must be strings")?,
            );
        }
    }
    if let Some(env) = adapter.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            if key.contains('=') || key.contains('\0') {
                return Err("adapter environment contains invalid key".into());
            }
            process.env(
                key,
                value.as_str().ok_or("adapter.env values must be strings")?,
            );
        }
    }
    let mut child = process
        .spawn()
        .map_err(|e| format!("failed to start DAP adapter: {e}"))?;
    let stdin = child.stdin.take().ok_or("DAP adapter stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .ok_or("DAP adapter stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("DAP adapter stderr unavailable")?;
    let session = Arc::new(DapSession {
        stdin: AsyncMutex::new(stdin),
        child: AsyncMutex::new(child),
        pending: Mutex::new(HashMap::new()),
        events: Mutex::new(Vec::new()),
        stderr: Mutex::new(String::new()),
        sequence: std::sync::atomic::AtomicU64::new(1),
        dead: CancellationToken::new(),
        reader: Mutex::new(None),
        stderr_reader: Mutex::new(None),
        _resource: ctx.register_resource(ResourceKind::Subprocess, "dap-adapter"),
    });
    let reader_session = session.clone();
    *lock(&session.reader) = Some(tokio::spawn(async move {
        read_loop(stdout, reader_session).await
    }));
    let stderr_session = session.clone();
    *lock(&session.stderr_reader) = Some(tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => append_bounded(&mut lock(&stderr_session.stderr), &chunk[..n]),
            }
        }
    }));
    Ok(session)
}

async fn read_loop(stdout: impl AsyncRead + Unpin, session: Arc<DapSession>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(message)) => route_message(&session, message),
            Ok(None) => {
                fail_pending(&session.pending, "DAP adapter closed its protocol stream");
                break;
            }
            Err(error) => {
                fail_pending(&session.pending, &error);
                lock(&session.stderr).push_str(&error);
                break;
            }
        }
    }
    session.dead.cancel();
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>, String> {
    let mut content_length = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("DAP header read failed: {e}"))?;
        if n == 0 {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err("truncated DAP header".into())
            };
        }
        header_bytes = header_bytes.saturating_add(n);
        if header_bytes > MAX_HEADER_BYTES {
            return Err("DAP header exceeds 65536 byte limit".into());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line
            .trim_end_matches(['\r', '\n'])
            .split_once(':')
            .ok_or("malformed DAP header")?;
        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid DAP Content-Length")?,
            );
        }
    }
    let len = content_length.ok_or("DAP frame missing Content-Length")?;
    if len > MAX_FRAME_BYTES {
        return Err(format!("DAP frame exceeds {MAX_FRAME_BYTES} byte limit"));
    }
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("truncated DAP frame: {e}"))?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| format!("malformed DAP JSON: {e}"))
}

fn route_message(session: &DapSession, message: Value) {
    if message.get("type").and_then(Value::as_str) == Some("response") {
        if let Some(id) = message.get("request_seq").and_then(Value::as_u64) {
            if let Some(sender) = lock(&session.pending).remove(&id) {
                let success = message
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result = if success {
                    Ok(message)
                } else {
                    Err(message
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("DAP request failed")
                        .to_string())
                };
                let _ = sender.send(result);
            }
        }
    } else {
        let mut events = lock(&session.events);
        if events.len() == MAX_EVENTS {
            events.remove(0);
        }
        events.push(message);
    }
}
fn fail_pending(
    pending: &Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    error: &str,
) {
    for (_, sender) in std::mem::take(&mut *lock(pending)) {
        let _ = sender.send(Err(error.to_string()));
    }
}

fn append_bounded(output: &mut String, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let remaining = MAX_OUTPUT_BYTES.saturating_sub(output.len());
    output.push_str(&text[..text.floor_char_boundary(remaining.min(text.len()))]);
}

fn validate_command(command: &str, workspace: &Path) -> Result<PathBuf, String> {
    if command.is_empty() || command.contains('\0') {
        return Err("adapter.command is invalid".into());
    }
    // Bare PATH names must be known debug adapters (CORE_REVIEW).
    const DAP_ALLOWLIST: &[&str] = &[
        "python",
        "python3",
        "node",
        "nodejs",
        "dlv",
        "lldb",
        "lldb-vscode",
        "lldb-dap",
        "gdb",
        "codelldb",
        "rust-gdb",
        "rust-lldb",
        "netcoredbg",
        "vsdbg",
        "js-debug-adapter",
        "rdbg",
    ];
    if Path::new(command).components().count() == 1 {
        let base = Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(command);
        if !DAP_ALLOWLIST.iter().any(|a| *a == base) {
            return Err(format!(
                "debug adapter '{command}' is not allowlisted; use a workspace-relative path or one of: {}",
                DAP_ALLOWLIST.join(", ")
            ));
        }
        return Ok(PathBuf::from(command));
    }
    resolve_workspace_path(workspace, command)
}

fn resolve_workspace_path(workspace: &Path, input: &str) -> Result<PathBuf, String> {
    let path = Path::new(input);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("path escapes workspace: {input}"));
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("workspace unavailable: {e}"))?;
    let canonical = joined
        .canonicalize()
        .map_err(|e| format!("debug path {input:?} unavailable: {e}"))?;
    if !canonical.starts_with(&workspace) {
        return Err(format!("path outside workspace: {input}"));
    }
    Ok(canonical)
}

fn validate_debug_arguments(
    action: &str,
    args: Option<&Value>,
    workspace: &Path,
) -> Result<(), String> {
    if !matches!(action, "launch" | "attach") {
        return Ok(());
    }
    fn visit(value: &Value, workspace: &Path) -> Result<(), String> {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if matches!(key.as_str(), "cwd" | "program" | "executable" | "module") {
                        if let Some(path) = value.as_str() {
                            resolve_workspace_path(workspace, path)?;
                        }
                    } else {
                        visit(value, workspace)?;
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, workspace)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    if let Some(args) = args {
        visit(args, workspace)?;
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub async fn shutdown_session(session_id: &str) {
    let session = lock(sessions()).remove(session_id);
    if let Some(session) = session {
        session.stop().await;
    }
}

pub async fn shutdown_all() {
    let all: Vec<_> = lock(sessions())
        .drain()
        .map(|(_, session)| session)
        .collect();
    for session in all {
        session.stop().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn frame_parser_rejects_malformed_and_oversize_frames() {
        let malformed = b"Bad\r\n\r\n{}".to_vec();
        assert!(read_frame(&mut BufReader::new(malformed.as_slice()))
            .await
            .unwrap_err()
            .contains("malformed"));
        let oversized = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        assert!(read_frame(&mut BufReader::new(oversized.as_bytes()))
            .await
            .unwrap_err()
            .contains("exceeds"));
    }

    #[tokio::test]
    async fn fixture_protocol_routes_initialize_launch_breakpoint_stack_disconnect() {
        let script = r#"import json,sys
while True:
 h={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line in (b'\r\n',b'\n'): break
  k,v=line.decode().split(':',1); h[k.lower()]=v.strip()
 m=json.loads(sys.stdin.buffer.read(int(h['content-length'])))
 cmd=m['command']; body={}
 if cmd=='stackTrace': body={'stackFrames':[{'id':1,'name':'fixture','line':7,'column':1}], 'totalFrames':1}
 r={'seq':m['seq']+100,'type':'response','request_seq':m['seq'],'success':True,'command':cmd,'body':body}
 b=json.dumps(r,separators=(',',':')).encode(); sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(b)).encode()+b); sys.stdout.buffer.flush()
 if cmd=='disconnect': sys.exit(0)
"#;
        let dir = std::env::temp_dir().join(format!("catcode-dap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.py");
        std::fs::write(&path, script).unwrap();
        let mut child = Command::new("python3")
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let session = Arc::new(DapSession {
            stdin: AsyncMutex::new(stdin),
            child: AsyncMutex::new(child),
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            stderr: Mutex::new(String::new()),
            sequence: std::sync::atomic::AtomicU64::new(1),
            dead: CancellationToken::new(),
            reader: Mutex::new(None),
            stderr_reader: Mutex::new(None),
            _resource: None,
        });
        let cloned = session.clone();
        *lock(&session.reader) = Some(tokio::spawn(async move { read_loop(stdout, cloned).await }));
        let cancel = CancellationToken::new();
        for command in [
            "initialize",
            "launch",
            "setBreakpoints",
            "stackTrace",
            "disconnect",
        ] {
            let response = session
                .request(command, json!({}), Duration::from_secs(2), &cancel)
                .await
                .unwrap();
            assert_eq!(response["command"], command);
            if command == "stackTrace" {
                assert_eq!(response["body"]["stackFrames"][0]["name"], "fixture");
            }
        }
        session.stop().await;
        let status = session.child.lock().await.wait().await.unwrap();
        assert!(!status.success());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn process_death_is_bounded() {
        let mut child = Command::new("python3")
            .args(["-c", "import sys; sys.exit(3)"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let session = Arc::new(DapSession {
            stdin: AsyncMutex::new(stdin),
            child: AsyncMutex::new(child),
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            stderr: Mutex::new(String::new()),
            sequence: std::sync::atomic::AtomicU64::new(1),
            dead: CancellationToken::new(),
            reader: Mutex::new(None),
            stderr_reader: Mutex::new(None),
            _resource: None,
        });
        let cloned = session.clone();
        *lock(&session.reader) = Some(tokio::spawn(async move { read_loop(stdout, cloned).await }));
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            session.request(
                "initialize",
                json!({}),
                Duration::from_secs(1),
                &CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.contains("adapter") || error.contains("protocol") || error.contains("write"));
        session.stop().await;
        assert!(session.child.lock().await.try_wait().unwrap().is_some());
    }
}
