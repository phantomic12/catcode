//! Shared Chromium DevTools Protocol backend.
//!
//! A single supervised Chromium process owns a remote-debugging endpoint. Each
//! browser session gets an incognito CDP browser context and target, preventing
//! cookies, storage, and navigation state from crossing sessions.

use super::backend::{create_response, require_session_id, str_arg};
use super::{next_snapshot_id, ok_envelope, BrowserError};
use crate::config::Config;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};

const MAX_CDP_FRAME: usize = 8 * 1024 * 1024;
fn frame_size(message: &Message) -> usize {
    match message {
        Message::Text(text) => text.len(),
        Message::Binary(bytes) => bytes.len(),
        Message::Ping(bytes) | Message::Pong(bytes) => bytes.len(),
        Message::Close(Some(frame)) => frame.reason.len() + 2,
        _ => 0,
    }
}

fn reject_oversized_frame(message: &Message) -> Result<(), BrowserError> {
    if frame_size(message) > MAX_CDP_FRAME {
        Err(BrowserError::new(
            "CDP_FRAME_TOO_LARGE",
            "Chromium CDP frame exceeds 8 MiB limit",
        ))
    } else {
        Ok(())
    }
}
const CDP_TIMEOUT: Duration = Duration::from_secs(20);
fn cdp_socket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_send_queue: None,
        write_buffer_size: 128 * 1024,
        max_write_buffer_size: MAX_CDP_FRAME,
        max_message_size: Some(MAX_CDP_FRAME),
        max_frame_size: Some(MAX_CDP_FRAME),
        accept_unmasked_frames: false,
    }
}
static ID_SEQ: AtomicU64 = AtomicU64::new(1);
static RUNTIME: LazyLock<Mutex<Runtime>> = LazyLock::new(|| Mutex::new(Runtime::default()));

struct Process {
    child: Child,
    endpoint: String,
    user_data_dir: PathBuf,
}
#[derive(Clone)]
struct Session {
    context_id: String,
    target_id: String,
    tab_id: String,
}
struct Runtime {
    process: Option<Process>,
    sessions: HashMap<String, Session>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            process: None,
            sessions: HashMap::new(),
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.child.kill();
            let _ = process.child.wait();
            let _ = std::fs::remove_dir_all(process.user_data_dir);
        }
    }
}

fn runtime() -> &'static Mutex<Runtime> {
    &RUNTIME
}

fn chromium_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CATALYST_CHROMIUM_PATH") {
        return Some(path.into());
    }
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ]
    .iter()
    .find_map(|name| which(name))
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}
fn devtools_endpoint(active_port: &str) -> Result<String, BrowserError> {
    let mut lines = active_port
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let port = lines
        .next()
        .ok_or_else(|| BrowserError::new("RUNTIME_START", "DevToolsActivePort missing port"))?;
    let path = lines.next().ok_or_else(|| {
        BrowserError::new("RUNTIME_START", "DevToolsActivePort missing browser path")
    })?;
    if !port.chars().all(|ch| ch.is_ascii_digit())
        || !path.starts_with("/devtools/browser/")
        || path.chars().any(|ch| ch.is_ascii_whitespace())
    {
        return Err(BrowserError::new(
            "RUNTIME_START",
            "DevToolsActivePort contains an invalid browser endpoint",
        ));
    }
    Ok(format!("ws://127.0.0.1:{port}{path}"))
}

fn start_process() -> Result<Process, BrowserError> {
    let binary = chromium_binary().ok_or_else(|| BrowserError::new(
        "BROWSER_UNAVAILABLE", "Chromium was not found. Install Chromium or set CATALYST_CHROMIUM_PATH; use the explicit native-browser WRY fallback only when Chromium is unavailable."
    ))?;
    let nonce = format!(
        "catalyst-cdp-{}-{}",
        std::process::id(),
        ID_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let user_data_dir = std::env::temp_dir().join(nonce);
    std::fs::create_dir_all(&user_data_dir)
        .map_err(|e| BrowserError::new("RUNTIME_START", format!("create Chromium profile: {e}")))?;
    let mut child = Command::new(binary)
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-sync",
        ])
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| BrowserError::new("RUNTIME_START", format!("start Chromium: {e}")))?;
    let active = user_data_dir.join("DevToolsActivePort");
    for _ in 0..100 {
        if let Ok(content) = std::fs::read_to_string(&active) {
            if let Ok(endpoint) = devtools_endpoint(&content) {
                return Ok(Process {
                    child,
                    endpoint,
                    user_data_dir,
                });
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&user_data_dir);
    Err(BrowserError::new(
        "RUNTIME_START",
        "Chromium did not expose a DevTools endpoint",
    ))
}

async fn call(endpoint: &str, method: &str, params: Value) -> Result<Value, BrowserError> {
    call_in_session(endpoint, method, params, None).await
}

async fn call_in_session(
    endpoint: &str,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Result<Value, BrowserError> {
    let (mut socket, _) = tokio::time::timeout(
        CDP_TIMEOUT,
        connect_async_with_config(endpoint, Some(cdp_socket_config()), false),
    )
    .await
    .map_err(|_| BrowserError::new("CDP_TIMEOUT", "connecting to Chromium timed out"))?
    .map_err(|e| BrowserError::new("CDP_CONNECT", format!("connect to Chromium: {e}")))?;
    let request =
        json!({"id": 1, "method": method, "params": params, "sessionId": session_id}).to_string();
    if request.len() > MAX_CDP_FRAME {
        return Err(BrowserError::new(
            "CDP_FRAME_TOO_LARGE",
            "CDP request exceeds 8 MiB limit",
        ));
    }
    let request_message = Message::Text(request.into());
    reject_oversized_frame(&request_message)?;
    socket
        .send(request_message)
        .await
        .map_err(|e| BrowserError::new("CDP_SEND", format!("send {method}: {e}")))?;
    loop {
        let message = tokio::time::timeout(CDP_TIMEOUT, socket.next())
            .await
            .map_err(|_| BrowserError::new("CDP_TIMEOUT", format!("{method} timed out")))?
            .ok_or_else(|| BrowserError::new("CDP_CLOSED", "Chromium closed the CDP connection"))?
            .map_err(|e| BrowserError::new("CDP_RECEIVE", format!("receive {method}: {e}")))?;
        reject_oversized_frame(&message)?;
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_CDP_FRAME {
            return Err(BrowserError::new(
                "CDP_FRAME_TOO_LARGE",
                "Chromium sent a CDP frame over 8 MiB",
            ));
        }
        let response: Value = serde_json::from_str(&text)
            .map_err(|e| BrowserError::new("CDP_PROTOCOL", format!("invalid CDP response: {e}")))?;
        if response.get("id") != Some(&json!(1)) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(BrowserError::new("CDP_ERROR", error.to_string()));
        }
        return response
            .get("result")
            .cloned()
            .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "CDP response has no result"));
    }
}

fn endpoint() -> Result<String, BrowserError> {
    let mut rt = runtime()
        .lock()
        .map_err(|_| BrowserError::new("INTERNAL", "CDP runtime lock poisoned"))?;
    if rt
        .process
        .as_mut()
        .is_some_and(|p| p.child.try_wait().ok().flatten().is_some())
    {
        rt.process = None;
        rt.sessions.clear();
    }
    if rt.process.is_none() {
        rt.process = Some(start_process()?);
    }
    Ok(rt.process.as_ref().expect("set above").endpoint.clone())
}

fn allowed_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://") || lower == "about:blank"
}

fn session(session_id: &str) -> Result<Session, BrowserError> {
    runtime()
        .lock()
        .map_err(|_| BrowserError::new("INTERNAL", "CDP runtime lock poisoned"))?
        .sessions
        .get(session_id)
        .cloned()
        .ok_or_else(|| BrowserError::new("SESSION_NOT_FOUND", format!("no session {session_id}")))
}

async fn target_call(
    session: &Session,
    method: &str,
    params: Value,
) -> Result<Value, BrowserError> {
    let endpoint = endpoint()?;
    let (mut socket, _) = tokio::time::timeout(
        CDP_TIMEOUT,
        connect_async_with_config(&endpoint, Some(cdp_socket_config()), false),
    )
    .await
    .map_err(|_| BrowserError::new("CDP_TIMEOUT", "connecting to Chromium timed out"))?
    .map_err(|e| BrowserError::new("CDP_CONNECT", format!("connect to Chromium: {e}")))?;
    let attach = json!({"id":1,"method":"Target.attachToTarget","params":{"targetId":session.target_id,"flatten":true}}).to_string();
    if attach.len() > MAX_CDP_FRAME {
        return Err(BrowserError::new(
            "CDP_FRAME_TOO_LARGE",
            "CDP attach request exceeds 8 MiB limit",
        ));
    }
    let attach_message = Message::Text(attach.into());
    reject_oversized_frame(&attach_message)?;
    socket
        .send(attach_message)
        .await
        .map_err(|e| BrowserError::new("CDP_SEND", e.to_string()))?;
    let session_id = loop {
        let message = tokio::time::timeout(CDP_TIMEOUT, socket.next())
            .await
            .map_err(|_| BrowserError::new("CDP_TIMEOUT", "attach timed out"))?
            .ok_or_else(|| BrowserError::new("CDP_CLOSED", "Chromium closed CDP"))?
            .map_err(|e| BrowserError::new("CDP_RECEIVE", e.to_string()))?;
        reject_oversized_frame(&message)?;
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_CDP_FRAME {
            return Err(BrowserError::new(
                "CDP_FRAME_TOO_LARGE",
                "Chromium sent a CDP frame over 8 MiB",
            ));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| BrowserError::new("CDP_PROTOCOL", e.to_string()))?;
        if value["id"] == 1 {
            break value["result"]["sessionId"]
                .as_str()
                .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "attach missing sessionId"))?
                .to_string();
        }
    };
    let request =
        json!({"id":2,"method":method,"params":params,"sessionId":session_id}).to_string();
    if request.len() > MAX_CDP_FRAME {
        return Err(BrowserError::new(
            "CDP_FRAME_TOO_LARGE",
            "CDP request exceeds 8 MiB limit",
        ));
    }
    socket
        .send(Message::Text(request.into()))
        .await
        .map_err(|e| BrowserError::new("CDP_SEND", e.to_string()))?;
    loop {
        let message = tokio::time::timeout(CDP_TIMEOUT, socket.next())
            .await
            .map_err(|_| BrowserError::new("CDP_TIMEOUT", format!("{method} timed out")))?
            .ok_or_else(|| BrowserError::new("CDP_CLOSED", "Chromium closed CDP"))?
            .map_err(|e| BrowserError::new("CDP_RECEIVE", e.to_string()))?;
        reject_oversized_frame(&message)?;
        let Message::Text(text) = message else {
            continue;
        };
        if text.len() > MAX_CDP_FRAME {
            return Err(BrowserError::new(
                "CDP_FRAME_TOO_LARGE",
                "Chromium sent a CDP frame over 8 MiB",
            ));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| BrowserError::new("CDP_PROTOCOL", e.to_string()))?;
        if value["id"] != 2 {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(BrowserError::new("CDP_ERROR", error.to_string()));
        }
        return value
            .get("result")
            .cloned()
            .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "CDP response has no result"));
    }
}

async fn evaluate(session_id: &str, expression: String) -> Result<Value, BrowserError> {
    let entry = session(session_id)?;
    let result = target_call(
        &entry,
        "Runtime.evaluate",
        json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
    )
    .await?;
    if let Some(error) = result.get("exceptionDetails") {
        return Err(BrowserError::new("EVAL_FAILED", error.to_string()));
    }
    Ok(result["result"]["value"].clone())
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".into())
}

async fn snapshot(session_id: &str) -> Result<Value, BrowserError> {
    let entry = session(session_id)?;
    let value = evaluate(session_id, include_str!("bridge/snapshot.js").to_string()).await?;
    let snapshot_id = next_snapshot_id();
    let mut out = ok_envelope(session_id, &entry.tab_id, value);
    out["snapshot_id"] = json!(snapshot_id);
    Ok(out)
}

async fn wait_for(
    session_id: &str,
    script: String,
    timeout_ms: u64,
    poll_ms: u64,
) -> Result<Value, BrowserError> {
    let start = std::time::Instant::now();
    loop {
        if evaluate(session_id, script.clone())
            .await?
            .as_bool()
            .unwrap_or(false)
        {
            let entry = session(session_id)?;
            return Ok(ok_envelope(
                session_id,
                &entry.tab_id,
                json!({"matched":true,"approximate":false,"elapsed_ms":start.elapsed().as_millis()}),
            ));
        }
        if start.elapsed() >= Duration::from_millis(timeout_ms) {
            return Err(
                BrowserError::new("WAIT_TIMEOUT", "browser wait condition timed out").retryable(),
            );
        }
        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
}

async fn create(args: &Value, cfg: &Config) -> Result<Value, BrowserError> {
    let endpoint = endpoint()?;
    let context = call(&endpoint, "Target.createBrowserContext", json!({})).await?;
    let context_id = context["browserContextId"]
        .as_str()
        .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "create context missing id"))?
        .to_string();
    let target = call(
        &endpoint,
        "Target.createTarget",
        json!({"url": "about:blank", "browserContextId": context_id}),
    )
    .await?;
    let target_id = target["targetId"]
        .as_str()
        .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "create target missing id"))?
        .to_string();
    let sid = format!("browser_{:x}", ID_SEQ.fetch_add(1, Ordering::Relaxed));
    let tab_id = format!("tab_{:x}", ID_SEQ.fetch_add(1, Ordering::Relaxed));
    let downloads_rel = format!(".catalyst-code/browser-downloads/{sid}");
    let downloads = crate::workspace::resolve(&cfg.workspace, &downloads_rel)
        .map_err(|e| BrowserError::new("INVALID_ARGS", format!("download path: {e}")))?;
    std::fs::create_dir_all(&downloads).map_err(|e| {
        BrowserError::new("RUNTIME_START", format!("create download directory: {e}"))
    })?;
    call(&endpoint, "Browser.setDownloadBehavior", json!({"behavior":"allow","downloadPath":downloads,"browserContextId":context_id,"eventsEnabled":true})).await?;
    runtime()
        .lock()
        .map_err(|_| BrowserError::new("INTERNAL", "CDP runtime lock poisoned"))?
        .sessions
        .insert(
            sid.clone(),
            Session {
                context_id,
                target_id,
                tab_id: tab_id.clone(),
            },
        );
    let mut out = create_response(
        &sid,
        &tab_id,
        args.get("profile")
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("ephemeral"),
    );
    out["backend"] = json!("chromium-cdp");
    out["downloads_path"] = json!(downloads_rel);
    out["capabilities"] = json!({"screenshots": true, "downloads": true, "file_uploads": false, "network_observation": "cdp", "native_accessibility": false, "visual_mode": false, "shared_process": true, "isolated_contexts": true});
    Ok(out)
}
async fn close(session_id: &str) -> Result<Value, BrowserError> {
    let entry = runtime()
        .lock()
        .map_err(|_| BrowserError::new("INTERNAL", "CDP runtime lock poisoned"))?
        .sessions
        .remove(session_id)
        .ok_or_else(|| {
            BrowserError::new("SESSION_NOT_FOUND", format!("no session {session_id}"))
        })?;
    let endpoint = endpoint()?;
    let _ = call(
        &endpoint,
        "Target.closeTarget",
        json!({"targetId": entry.target_id}),
    )
    .await;
    let _ = call(
        &endpoint,
        "Target.disposeBrowserContext",
        json!({"browserContextId": entry.context_id}),
    )
    .await;
    Ok(json!({"success": true, "session_id": session_id, "closed": true}))
}

async fn navigate(session_id: &str, url: &str) -> Result<Value, BrowserError> {
    if !allowed_url(url) {
        return Err(BrowserError::new(
            "NAVIGATION_DENIED",
            "only http://, https://, and about:blank navigation is allowed",
        ));
    }
    let entry = session(session_id)?;
    target_call(&entry, "Page.navigate", json!({"url": url})).await?;
    Ok(ok_envelope(
        session_id,
        &entry.tab_id,
        json!({"url": url, "navigation": {"started": true}}),
    ))
}

fn confined_path(
    workspace: &Path,
    requested: Option<&str>,
    session_id: &str,
) -> Result<(PathBuf, String), BrowserError> {
    let rel = requested
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            format!(
                ".catalyst-code/browser-screenshots/{session_id}-{}.png",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            )
        });
    if !rel.ends_with(".png") {
        return Err(BrowserError::new(
            "INVALID_ARGS",
            "screenshots must use a .png path",
        ));
    }
    let abs = crate::workspace::resolve(workspace, &rel)
        .map_err(|e| BrowserError::new("INVALID_ARGS", format!("screenshot path: {e}")))?;
    Ok((abs, rel))
}
pub async fn dispatch(name: &str, args: &Value, cfg: &Config) -> Result<Value, BrowserError> {
    match name {
        "browser_create" => create(args, cfg).await,
        "browser_close" => close(require_session_id(args)?).await,
        "browser_list_sessions" => {
            let rt = runtime()
                .lock()
                .map_err(|_| BrowserError::new("INTERNAL", "CDP runtime lock poisoned"))?;
            Ok(
                json!({"success":true,"backend":"chromium-cdp","processes":usize::from(rt.process.is_some()),"sessions":rt.sessions.iter().map(|(id,s)|json!({"session_id":id,"tab_id":s.tab_id,"context_id":s.context_id})).collect::<Vec<_>>() }),
            )
        }
        "browser_navigate" => {
            navigate(
                require_session_id(args)?,
                str_arg(args, "url")
                    .ok_or_else(|| BrowserError::new("INVALID_ARGS", "url is required"))?,
            )
            .await
        }
        "browser_back" => {
            let sid = require_session_id(args)?;
            let entry = session(sid)?;
            evaluate(sid, "history.back();true".into()).await?;
            Ok(ok_envelope(
                sid,
                &entry.tab_id,
                json!({"navigation":{"started":true}}),
            ))
        }
        "browser_reload" => {
            let sid = require_session_id(args)?;
            let entry = session(sid)?;
            target_call(&entry,"Page.reload",json!({"ignoreCache":args.get("ignore_cache").and_then(Value::as_bool).unwrap_or(false)})).await?;
            Ok(ok_envelope(
                sid,
                &entry.tab_id,
                json!({"navigation":{"started":true}}),
            ))
        }
        "browser_snapshot" => snapshot(require_session_id(args)?).await,
        "browser_find" => {
            let sid = require_session_id(args)?;
            snapshot(sid).await?;
            let query = args
                .get("query")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "query is required"))?;
            let strategy = query
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("text");
            let value = query.get("value").and_then(Value::as_str).unwrap_or("");
            let matches = evaluate(
                sid,
                format!(
                    "window.__cc_find_strategy={};window.__cc_find_value={};{}",
                    js_string(strategy),
                    js_string(value),
                    include_str!("bridge/find.js")
                ),
            )
            .await?;
            let entry = session(sid)?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"matches":matches})))
        }
        "browser_click" => {
            let sid = require_session_id(args)?;
            let r = str_arg(args, "ref")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "ref is required"))?;
            let value=evaluate(sid,format!("(()=>{{const e=document.querySelector('[data-catalyst-ref='+{}+']');if(!e)throw new Error('ELEMENT_STALE');e.scrollIntoView({{block:'center'}});e.click();return true}})()",js_string(r))).await?;
            let entry = session(sid)?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"clicked":value})))
        }
        "browser_fill" | "browser_type" => {
            let sid = require_session_id(args)?;
            let r = str_arg(args, "ref")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "ref is required"))?;
            let text = str_arg(args, "text").unwrap_or("");
            let append = name == "browser_type"
                && !args
                    .get("clear_first")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            let script=format!("(()=>{{const e=document.querySelector('[data-catalyst-ref='+{}+']');if(!e)throw new Error('ELEMENT_STALE');e.focus();e.value={} ? String(e.value||'')+{} : {};e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return e.value}})()",js_string(r),append,js_string(text),js_string(text));
            let value = evaluate(sid, script).await?;
            let entry = session(sid)?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"value":value})))
        }
        "browser_press" => {
            let sid = require_session_id(args)?;
            let key = str_arg(args, "key")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "key is required"))?;
            let entry = session(sid)?;
            target_call(
                &entry,
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","key":key}),
            )
            .await?;
            target_call(
                &entry,
                "Input.dispatchKeyEvent",
                json!({"type":"keyUp","key":key}),
            )
            .await?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"key":key})))
        }
        "browser_scroll" => {
            let sid = require_session_id(args)?;
            let direction = str_arg(args, "direction").unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_f64).unwrap_or(600.0);
            let (x, y) = match direction {
                "up" => (0.0, -amount),
                "left" => (-amount, 0.0),
                "right" => (amount, 0.0),
                _ => (0.0, amount),
            };
            evaluate(sid, format!("window.scrollBy({x},{y});true")).await?;
            let entry = session(sid)?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"scrolled":true})))
        }
        "browser_wait" => {
            let sid = require_session_id(args)?;
            let condition = args
                .get("condition")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "condition is required"))?;
            let kind = condition
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("timeout");
            let value = condition.get("value").and_then(Value::as_str).unwrap_or("");
            let script = match kind {
                "text" => format!(
                    "!!document.body&&document.body.innerText.includes({})",
                    js_string(value)
                ),
                "url" => format!("location.href.includes({})", js_string(value)),
                "javascript" => condition
                    .get("script")
                    .and_then(Value::as_str)
                    .unwrap_or("false")
                    .to_string(),
                "dom_stable" => "document.readyState==='complete'".into(),
                "timeout" => {
                    tokio::time::sleep(Duration::from_millis(
                        args.get("timeout_ms")
                            .and_then(Value::as_u64)
                            .unwrap_or(1000),
                    ))
                    .await;
                    "true".into()
                }
                _ => "false".into(),
            };
            wait_for(
                sid,
                script,
                args.get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000)
                    .min(60_000),
                args.get("poll_interval_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .max(20),
            )
            .await
        }
        "browser_evaluate" => {
            let sid = require_session_id(args)?;
            let value = evaluate(
                sid,
                str_arg(args, "script")
                    .ok_or_else(|| BrowserError::new("INVALID_ARGS", "script is required"))?
                    .to_string(),
            )
            .await?;
            let entry = session(sid)?;
            Ok(ok_envelope(sid, &entry.tab_id, json!({"value":value})))
        }
        "browser_screenshot" => {
            let sid = require_session_id(args)?;
            let entry = session(sid)?;
            let (path, rel) = confined_path(&cfg.workspace, str_arg(args, "path"), sid)?;
            std::fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| BrowserError::new("INVALID_ARGS", "screenshot has no parent"))?,
            )
            .map_err(|e| BrowserError::new("SCREENSHOT_FAILED", e.to_string()))?;
            let result=target_call(&entry,"Page.captureScreenshot",json!({"format":"png","captureBeyondViewport":args.get("target").and_then(|v|v.get("type")).and_then(Value::as_str)==Some("full_page")})).await?;
            let data = result["data"]
                .as_str()
                .ok_or_else(|| BrowserError::new("CDP_PROTOCOL", "screenshot data missing"))?;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
                .map_err(|e| BrowserError::new("SCREENSHOT_FAILED", e.to_string()))?;
            std::fs::write(&path, &bytes)
                .map_err(|e| BrowserError::new("SCREENSHOT_FAILED", e.to_string()))?;
            Ok(ok_envelope(
                sid,
                &entry.tab_id,
                json!({"path":rel,"bytes":bytes.len()}),
            ))
        }
        "browser_show" | "browser_hide" => {
            let sid = require_session_id(args)?;
            let entry = session(sid)?;
            Ok(ok_envelope(
                sid,
                &entry.tab_id,
                json!({"visible":false,"warnings":["Chromium CDP is headless; enable native-browser explicitly for a visible WRY window"]}),
            ))
        }
        other => Err(BrowserError::new(
            "UNKNOWN_TOOL",
            format!("unhandled browser tool: {other}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn navigation_allowlist_rejects_unsafe_schemes() {
        assert!(allowed_url("https://example.com"));
        assert!(allowed_url("about:blank"));
        assert!(!allowed_url("file:///etc/passwd"));
        assert!(!allowed_url("javascript:alert(1)"));
        assert!(!allowed_url("data:text/html,x"));
    }
    #[test]
    fn devtools_active_port_uses_real_browser_path() {
        assert_eq!(
            devtools_endpoint("9222\n/devtools/browser/abc-123\n").unwrap(),
            "ws://127.0.0.1:9222/devtools/browser/abc-123"
        );
        assert!(devtools_endpoint("9222").is_err());
        assert!(devtools_endpoint("9222\n/devtools/page/abc\n").is_err());
    }
    #[test]
    fn screenshot_paths_are_workspace_confined() {
        let root = std::env::temp_dir().join("cdp-path-test");
        let (_, rel) = confined_path(&root, Some("shots/a.png"), "s").unwrap();
        assert_eq!(rel, "shots/a.png");
        assert!(confined_path(&root, Some("../escape.png"), "s").is_err());
    }
    #[tokio::test]
    #[ignore = "requires installed Chromium"]
    async fn installed_chromium_shares_process_and_isolates_contexts() {
        if chromium_binary().is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("catalyst-cdp-smoke-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Config {
            workspace: root.clone(),
            ..Config::default()
        };
        let a = create(&json!({}), &cfg).await.unwrap();
        let b = create(&json!({}), &cfg).await.unwrap();
        assert_ne!(a["session_id"], b["session_id"]);
        let listed = dispatch("browser_list_sessions", &json!({}), &cfg)
            .await
            .unwrap();
        assert_eq!(listed["processes"], 1);
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_ne!(sessions[0]["context_id"], sessions[1]["context_id"]);
        close(a["session_id"].as_str().unwrap()).await.unwrap();
        close(b["session_id"].as_str().unwrap()).await.unwrap();
        assert_eq!(
            dispatch("browser_list_sessions", &json!({}), &cfg)
                .await
                .unwrap()["sessions"],
            json!([])
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
