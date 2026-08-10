//! MCP client primitives. The dispatcher owns when this module is loaded/called.
//! This module deliberately keeps transport, protocol bounds, and safety policy
//! independent of the model/tool schema.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
const MAX_SURFACE_RESULT_BYTES: usize = 1024 * 1024;
const SECRET_NAMES: &[&str] = &[
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "auth",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum TransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpConfig {
    pub name: String,
    #[serde(flatten)]
    pub transport: TransportConfig,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}
fn default_timeout() -> u64 {
    30_000
}
fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP transport: {0}")]
    Transport(String),
    #[error("MCP protocol error {code}: {message}")]
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("MCP response exceeded {0} bytes")]
    TooLarge(usize),
    #[error("MCP request timed out")]
    Timeout,
    #[error("MCP request cancelled")]
    Cancelled,
    #[error("invalid MCP response: {0}")]
    Invalid(String),
}
#[derive(Debug, Error)]
pub enum McpSurfaceError {
    #[error("invalid mcp tool arguments: {0}")]
    InvalidArguments(String),
    #[error("MCP server '{0}' is not configured in trusted user config")]
    AbsentServer(String),
    #[error(transparent)]
    Client(#[from] McpError),
}

/// Execute the model-facing MCP surface using only a named, trusted config entry.
/// Transport details never come from tool arguments.
pub async fn execute(
    servers: &[McpConfig],
    args: &Value,
    cancel: &CancellationToken,
) -> Result<Value, McpSurfaceError> {
    let obj = args
        .as_object()
        .ok_or_else(|| McpSurfaceError::InvalidArguments("expected an object".to_string()))?;
    const ALLOWED: &[&str] = &["action", "server", "tool", "arguments"];
    if let Some(key) = obj.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(McpSurfaceError::InvalidArguments(format!(
            "unsupported field '{key}'; transport command, env, headers, and URL are config-only"
        )));
    }
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| McpSurfaceError::InvalidArguments("missing string 'action'".into()))?;
    if !matches!(action, "list" | "call") {
        return Err(McpSurfaceError::InvalidArguments(
            "action must be 'list' or 'call'".into(),
        ));
    }
    let server_name = obj
        .get("server")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| McpSurfaceError::InvalidArguments("missing non-empty 'server'".into()))?;
    let configured = servers
        .iter()
        .find(|server| server.name == server_name)
        .cloned()
        .ok_or_else(|| McpSurfaceError::AbsentServer(server_name.to_string()))?;
    let mut configured = configured;
    configured.max_bytes = configured.max_bytes.min(MAX_SURFACE_RESULT_BYTES);
    let mut client = McpClient::connect(configured).await?;
    match action {
        "list" => {
            if obj.contains_key("tool") || obj.contains_key("arguments") {
                return Err(McpSurfaceError::InvalidArguments(
                    "list accepts only action and server".into(),
                ));
            }
            Ok(json!({ "tools": client.list_tools(Some(cancel)).await? }))
        }
        "call" => {
            let tool = obj
                .get("tool")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    McpSurfaceError::InvalidArguments("call requires non-empty 'tool'".into())
                })?;
            let arguments = obj.get("arguments").cloned().unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return Err(McpSurfaceError::InvalidArguments(
                    "'arguments' must be an object".into(),
                ));
            }
            let argument_bytes = serde_json::to_vec(&arguments)
                .map_err(|e| McpSurfaceError::InvalidArguments(e.to_string()))?
                .len();
            if argument_bytes > MAX_TOOL_ARGUMENT_BYTES {
                return Err(McpSurfaceError::InvalidArguments(format!(
                    "arguments exceed {MAX_TOOL_ARGUMENT_BYTES} bytes"
                )));
            }
            client
                .call_tool(tool, arguments, Some(cancel))
                .await
                .map_err(Into::into)
        }
        _ => unreachable!(),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: Option<String>,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(default)]
    pub server_info: Value,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalClass {
    ReadOnly,
    Mutating,
    Destructive,
}

/// Conservative classification: remote tools are never silently treated as safe.
pub fn classify_tool(name: &str, description: Option<&str>, args: &Value) -> ApprovalClass {
    let s = format!("{} {} {}", name, description.unwrap_or_default(), args).to_ascii_lowercase();
    if [
        "delete", "destroy", "drop", "remove", "write", "exec", "shell", "send", "publish",
        "transfer",
    ]
    .iter()
    .any(|x| s.contains(x))
    {
        ApprovalClass::Destructive
    } else if [
        "create", "update", "edit", "modify", "move", "install", "set", "upload",
    ]
    .iter()
    .any(|x| s.contains(x))
    {
        ApprovalClass::Mutating
    } else {
        ApprovalClass::ReadOnly
    }
}

/// Remove likely secrets before exposing a configured environment to a child.
pub fn filter_environment(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(|(k, _)| {
            let l = k.to_ascii_lowercase();
            !SECRET_NAMES.iter().any(|x| l.contains(x))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

struct StdioTransport {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
enum Transport {
    Stdio(Arc<Mutex<StdioTransport>>),
    Http {
        client: reqwest::Client,
        url: String,
        headers: HashMap<String, String>,
    },
}
pub struct McpClient {
    transport: Transport,
    next_id: u64,
    max_bytes: usize,
    timeout: Duration,
    initialized: bool,
}

impl McpClient {
    pub async fn connect(config: McpConfig) -> Result<Self, McpError> {
        let max_bytes = config.max_bytes.min(64 * 1024 * 1024).max(1024);
        let transport = match config.transport {
            TransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut c = Command::new(command);
                c.args(args);
                c.env_clear();
                c.envs(filter_environment(&env));
                if let Some(d) = cwd {
                    c.current_dir(d);
                }
                let mut child = c
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| McpError::Transport(e.to_string()))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| McpError::Transport("missing stdin".into()))?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| McpError::Transport("missing stdout".into()))?;
                Transport::Stdio(Arc::new(Mutex::new(StdioTransport {
                    child,
                    stdin,
                    stdout: BufReader::new(stdout),
                })))
            }
            TransportConfig::Http { url, headers } => Transport::Http {
                client: reqwest::Client::new(),
                url,
                headers,
            },
        };
        let mut c = Self {
            transport,
            next_id: 1,
            max_bytes,
            timeout: Duration::from_millis(config.timeout_ms.clamp(100, 300_000)),
            initialized: false,
        };
        c.initialize().await?;
        Ok(c)
    }
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let fut = async {
            match &self.transport {
                Transport::Http {
                    client,
                    url,
                    headers,
                } => {
                    let mut r = client.post(url).json(&req);
                    for (k, v) in headers {
                        r = r.header(k, v);
                    }
                    let b = r
                        .send()
                        .await
                        .map_err(|e| McpError::Transport(e.to_string()))?
                        .bytes()
                        .await
                        .map_err(|e| McpError::Transport(e.to_string()))?;
                    if b.len() > self.max_bytes {
                        return Err(McpError::TooLarge(self.max_bytes));
                    }
                    serde_json::from_slice(&b).map_err(|e| McpError::Invalid(e.to_string()))
                }
                Transport::Stdio(t) => {
                    let mut t = t.lock().await;
                    let bytes = serde_json::to_vec(&req).unwrap();
                    t.stdin
                        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
                        .await
                        .map_err(|e| McpError::Transport(e.to_string()))?;
                    t.stdin
                        .write_all(&bytes)
                        .await
                        .map_err(|e| McpError::Transport(e.to_string()))?;
                    t.stdin
                        .flush()
                        .await
                        .map_err(|e| McpError::Transport(e.to_string()))?;
                    read_message(&mut t.stdout, self.max_bytes).await
                }
            }
        };
        let v = if let Some(tok) = cancel {
            tokio::select! { _=tok.cancelled()=>Err(McpError::Cancelled), x=timeout(self.timeout,fut)=>x.map_err(|_|McpError::Timeout).and_then(|x|x) }
        } else {
            timeout(self.timeout, fut)
                .await
                .map_err(|_| McpError::Timeout)
                .and_then(|x| x)
        }?;
        if let Some(e) = v.get("error") {
            return Err(McpError::Remote {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(-1),
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
                data: e.get("data").cloned(),
            });
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| McpError::Invalid("missing result".into()))
    }
    pub async fn initialize(&mut self) -> Result<InitializeResult, McpError> {
        let v=self.request("initialize",json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"catalyst-code","version":"0.2"}}),None).await?;
        let r: InitializeResult =
            serde_json::from_value(v).map_err(|e| McpError::Invalid(e.to_string()))?;
        let _ = self.notify("notifications/initialized", json!({})).await;
        self.initialized = true;
        Ok(r)
    }
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        if let Transport::Stdio(t) = &self.transport {
            let mut t = t.lock().await;
            let b = serde_json::to_vec(&json!({"jsonrpc":"2.0","method":method,"params":params}))
                .unwrap();
            t.stdin
                .write_all(format!("Content-Length: {}\r\n\r\n", b.len()).as_bytes())
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?;
            t.stdin
                .write_all(&b)
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?;
            t.stdin
                .flush()
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?;
        }
        Ok(())
    }
    pub async fn list_tools(
        &mut self,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<McpTool>, McpError> {
        let v = self.request("tools/list", json!({}), cancel).await?;
        serde_json::from_value(v.get("tools").cloned().unwrap_or_else(|| json!([])))
            .map_err(|e| McpError::Invalid(e.to_string()))
    }
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        cancel: Option<&CancellationToken>,
    ) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            json!({"name":name,"arguments":arguments}),
            cancel,
        )
        .await
    }
    pub async fn shutdown(&mut self) -> Result<(), McpError> {
        let _ = self.notify("notifications/cancelled", json!({})).await;
        if let Transport::Stdio(t) = &self.transport {
            let mut t = t.lock().await;
            t.child
                .kill()
                .await
                .map_err(|e| McpError::Transport(e.to_string()))?;
        }
        Ok(())
    }
}
async fn read_message(r: &mut BufReader<ChildStdout>, max: usize) -> Result<Value, McpError> {
    let mut len = None;
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line)
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?
            == 0
        {
            return Err(McpError::Transport("server closed stdout".into()));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = Some(
                v.trim()
                    .parse::<usize>()
                    .map_err(|_| McpError::Invalid("bad content length".into()))?,
            );
        }
    }
    let n = len.ok_or_else(|| McpError::Invalid("missing content length".into()))?;
    if n > max {
        return Err(McpError::TooLarge(max));
    }
    let mut b = vec![0; n];
    r.read_exact(&mut b)
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    serde_json::from_slice(&b).map_err(|e| McpError::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_filtered() {
        let e = HashMap::from([
            ("PATH".into(), "x".into()),
            ("API_TOKEN".into(), "y".into()),
        ]);
        assert!(filter_environment(&e).contains_key("PATH"));
        assert!(!filter_environment(&e).contains_key("API_TOKEN"));
    }
    #[test]
    fn conservative_approval() {
        assert_eq!(
            classify_tool("read", None, &json!({})),
            ApprovalClass::ReadOnly
        );
        assert_eq!(
            classify_tool("delete_file", None, &json!({})),
            ApprovalClass::Destructive
        );
    }
    #[tokio::test]
    async fn surface_rejects_transport_in_model_args() {
        let token = CancellationToken::new();
        let err = execute(
            &[],
            &json!({"action":"list","server":"x","command":"rm"}),
            &token,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("config-only"));
    }

    #[tokio::test]
    async fn surface_reports_absent_server_before_connecting() {
        let token = CancellationToken::new();
        let err = execute(&[], &json!({"action":"list","server":"missing"}), &token)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }
}
