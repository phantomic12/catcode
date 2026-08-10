//! WRY/tao native browser runtime (feature `native-browser`).
//!
//! Dedicated tao event-loop thread. JS results come back via
//! `evaluate_script_with_callback` (oneshot reply sent from the callback —
//! never block the event loop waiting).

use super::backend::{create_response, require_session_id, str_arg};
use super::{next_snapshot_id, ok_envelope, BrowserError};
use crate::config::Config;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, OnceLock};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Window, WindowBuilder};
use tokio::sync::oneshot;
use wry::WebViewBuilder;

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
use tao::platform::unix::EventLoopBuilderExtUnix;
#[cfg(target_os = "windows")]
use tao::platform::windows::EventLoopBuilderExtWindows;

type Reply = oneshot::Sender<Result<Value, BrowserError>>;

enum Cmd {
    Create {
        visible: bool,
        width: u32,
        height: u32,
        reply: Reply,
    },
    Close {
        session_id: String,
        reply: Reply,
    },
    List {
        reply: Reply,
    },
    Navigate {
        session_id: String,
        url: String,
        reply: Reply,
    },
    Back {
        session_id: String,
        reply: Reply,
    },
    Reload {
        session_id: String,
        reply: Reply,
    },
    /// Evaluate `script` and return the JSON-serialized JS result via callback.
    EvalJs {
        session_id: String,
        script: String,
        reply: Reply,
    },
    SetVisible {
        session_id: String,
        visible: bool,
        reply: Reply,
    },
    Screenshot {
        session_id: String,
        /// Absolute path to write PNG into (already workspace-resolved).
        path: String,
        /// "viewport" | "full_page"
        region: String,
        reply: Reply,
    },
}

struct TabState {
    window: Window,
    webview: wry::WebView,
    url: String,
    title: String,
}

struct Session {
    id: String,
    tab_id: String,
    visible: bool,
    last_activity: Instant,
    tab: TabState,
}

struct Runtime {
    sessions: HashMap<String, Session>,
}

static PROXY: OnceLock<EventLoopProxy<Cmd>> = OnceLock::new();
static ID_SEQ: AtomicU64 = AtomicU64::new(1);

fn ensure_runtime() -> Result<EventLoopProxy<Cmd>, BrowserError> {
    if let Some(p) = PROXY.get() {
        return Ok(p.clone());
    }
    let (ready_tx, ready_rx) = mpsc::channel::<Result<EventLoopProxy<Cmd>, String>>();
    std::thread::Builder::new()
        .name("catalyst-browser".into())
        .spawn(move || {
            // Headless hosts (no DISPLAY): auto-start Xvfb before GTK/tao init.
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            {
                match crate::browser::headless_display::ensure_display() {
                    Ok(info) => {
                        if info.auto_xvfb {
                            eprintln!(
                                "catalyst-browser: no DISPLAY — started Xvfb on {}",
                                info.display
                            );
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                }
            }
            let mut builder = EventLoopBuilder::<Cmd>::with_user_event();
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            builder.with_any_thread(true);
            #[cfg(target_os = "windows")]
            builder.with_any_thread(true);
            let event_loop = builder.build();

            let proxy = event_loop.create_proxy();
            let _ = ready_tx.send(Ok(proxy));
            let mut rt = Runtime {
                sessions: HashMap::new(),
            };
            event_loop.run(move |event, elwt, control_flow| {
                *control_flow = ControlFlow::Wait;
                match event {
                    Event::UserEvent(Cmd::Create {
                        visible,
                        width,
                        height,
                        reply,
                    }) => {
                        let _ = reply.send(create_session(&mut rt, elwt, visible, width, height));
                    }
                    Event::UserEvent(other) => handle_cmd(&mut rt, other),
                    Event::WindowEvent {
                        event: WindowEvent::CloseRequested,
                        window_id,
                        ..
                    } => {
                        if let Some(s) = rt
                            .sessions
                            .values_mut()
                            .find(|s| s.tab.window.id() == window_id)
                        {
                            s.tab.window.set_visible(false);
                            s.visible = false;
                        }
                    }
                    _ => {}
                }
            });
        })
        .map_err(|e| BrowserError::new("RUNTIME_START", format!("spawn browser thread: {e}")))?;

    let proxy = ready_rx
        .recv_timeout(Duration::from_secs(15))
        .map_err(|_| BrowserError::new("RUNTIME_START", "browser runtime thread did not start"))?
        .map_err(|e| BrowserError::new("RUNTIME_START", e))?;
    let _ = PROXY.set(proxy.clone());
    Ok(proxy)
}

fn handle_cmd(rt: &mut Runtime, cmd: Cmd) {
    match cmd {
        Cmd::Create { reply, .. } => {
            let _ = reply.send(Err(BrowserError::new(
                "INTERNAL",
                "Create needs EventLoopWindowTarget",
            )));
        }
        Cmd::Close { session_id, reply } => {
            let _ = reply.send(close_session(rt, &session_id));
        }
        Cmd::List { reply } => {
            let sessions: Vec<Value> = rt
                .sessions
                .values()
                .map(|s| {
                    json!({
                        "session_id": s.id,
                        "profile": "ephemeral",
                        "visible": s.visible,
                        "tabs": 1,
                        "active_tab_id": s.tab_id,
                        "url": s.tab.url,
                        "title": s.tab.title
                    })
                })
                .collect();
            let _ = reply.send(Ok(json!({ "success": true, "sessions": sessions })));
        }
        Cmd::Navigate {
            session_id,
            url,
            reply,
        } => {
            let _ = reply.send(navigate(rt, &session_id, &url));
        }
        Cmd::Back { session_id, reply } => {
            let sid = session_id.clone();
            match start_eval(
                rt,
                &session_id,
                "history.back(); true".into(),
                reply,
                move |v| {
                    ok_envelope(
                        &sid,
                        "tab_1",
                        json!({ "back": true, "eval": v, "snapshot_invalidated": true }),
                    )
                },
            ) {
                Ok(()) => {}
                Err((reply, e)) => {
                    let _ = reply.send(Err(e));
                }
            }
        }
        Cmd::Reload { session_id, reply } => {
            let _ = reply.send(reload(rt, &session_id));
        }
        Cmd::EvalJs {
            session_id,
            script,
            reply,
        } => match start_eval(rt, &session_id, script, reply, |v| v) {
            Ok(()) => {}
            Err((reply, e)) => {
                let _ = reply.send(Err(e));
            }
        },
        Cmd::SetVisible {
            session_id,
            visible,
            reply,
        } => {
            let _ = reply.send(set_visible(rt, &session_id, visible));
        }
        Cmd::Screenshot {
            session_id,
            path,
            region,
            reply,
        } => {
            start_screenshot(rt, &session_id, path, region, reply);
        }
    }
}

/// Start an evaluate_script_with_callback. On success the callback owns `reply`.
/// On immediate failure, returns (reply, error) so the caller can reply.
/// Start an evaluate_script_with_callback. On success the callback owns `reply`
/// (via Mutex/Option — wry requires `Fn`, not `FnOnce`). On immediate failure,
/// returns (reply, error) so the caller can reply.
fn start_eval<F: FnOnce(Value) -> Value + Send + 'static>(
    rt: &mut Runtime,
    session_id: &str,
    script: String,
    reply: Reply,
    map: F,
) -> Result<(), (Reply, BrowserError)> {
    let Some(s) = rt.sessions.get_mut(session_id) else {
        return Err((
            reply,
            BrowserError::new("SESSION_NOT_FOUND", format!("no session {session_id}")),
        ));
    };
    // Eval the script text so both expressions and statement IIFEs work.
    // Exceptions become JSON {__cc_error} (Windows ignores thrown exceptions).
    let script_json = serde_json::to_string(&script).unwrap_or_else(|_| String::from("\"null\""));
    let wrapped = format!(
        "(function(){{ try {{ return eval({script_json}); }} catch (e) {{ return {{ __cc_error: String(e && e.message ? e.message : e) }}; }} }})()"
    );
    s.last_activity = Instant::now();
    // wry::evaluate_script_with_callback takes Fn (may be invoked >1×) — take once.
    let reply_slot = std::sync::Arc::new(std::sync::Mutex::new(Some(reply)));
    let map_slot = std::sync::Arc::new(std::sync::Mutex::new(Some(map)));
    let reply_cb = reply_slot.clone();
    let map_cb = map_slot.clone();
    if let Err(e) = s
        .tab
        .webview
        .evaluate_script_with_callback(&wrapped, move |result_json| {
            let Ok(mut reply_guard) = reply_cb.lock() else {
                return;
            };
            let Some(reply) = reply_guard.take() else {
                return;
            };
            let map = map_cb.lock().ok().and_then(|mut g| g.take());
            let parsed: Value = serde_json::from_str(&result_json)
                .unwrap_or_else(|_| json!({ "value": result_json, "value_type": "string" }));
            if let Some(err) = parsed.get("__cc_error").and_then(|v| v.as_str()) {
                let code = if err.contains("ELEMENT_STALE") {
                    "ELEMENT_STALE"
                } else {
                    "EVAL_FAILED"
                };
                let mut be = BrowserError::new(code, err.to_string());
                if code == "ELEMENT_STALE" {
                    be = be.retryable();
                }
                let _ = reply.send(Err(be));
                return;
            }
            let out = match map {
                Some(f) => f(parsed),
                None => parsed,
            };
            let _ = reply.send(Ok(out));
        })
    {
        // Callback was never installed — recover reply from slot.
        let reply = reply_slot
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .expect("reply still in slot");
        return Err((reply, BrowserError::new("EVAL_FAILED", format!("{e}"))));
    }
    Ok(())
}

fn start_screenshot(
    rt: &mut Runtime,
    session_id: &str,
    path: String,
    region: String,
    reply: Reply,
) {
    let Some(s) = rt.sessions.get_mut(session_id) else {
        let _ = reply.send(Err(BrowserError::new(
            "SESSION_NOT_FOUND",
            format!("no session {session_id}"),
        )));
        return;
    };
    s.last_activity = Instant::now();
    let sid = s.id.clone();
    let tab = s.tab_id.clone();

    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        use webkit2gtk::{SnapshotOptions, SnapshotRegion, WebViewExt};
        use wry::WebViewExtUnix;

        // WebKitGTK snapshot fails on unrealized/hidden windows — show + pump paint.
        s.tab.window.set_visible(true);
        s.visible = true;
        {
            let ctx = glib::MainContext::default();
            for _ in 0..50 {
                while ctx.iteration(false) {}
            }
        }

        let wk = s.tab.webview.webview();
        let region_enum = if region == "full_page" || region == "full" {
            SnapshotRegion::FullDocument
        } else {
            SnapshotRegion::Visible
        };
        let target_type = if region == "full_page" || region == "full" {
            "full_page"
        } else {
            "viewport"
        }
        .to_string();
        let reply_slot = std::sync::Arc::new(std::sync::Mutex::new(Some(reply)));
        let reply_cb = reply_slot.clone();
        wk.snapshot(
            region_enum,
            SnapshotOptions::NONE,
            Option::<&gio::Cancellable>::None,
            move |result| {
                let Ok(mut guard) = reply_cb.lock() else {
                    return;
                };
                let Some(reply) = guard.take() else {
                    return;
                };
                match result {
                    Ok(surface) => match write_cairo_png(&surface, &path) {
                        Ok((width, height, bytes)) => {
                            let _ = reply.send(Ok(ok_envelope(
                                &sid,
                                &tab,
                                serde_json::json!({
                                    "path": path,
                                    "format": "png",
                                    "width": width,
                                    "height": height,
                                    "bytes": bytes,
                                    "target": { "type": target_type }
                                }),
                            )));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(BrowserError::new("SCREENSHOT_FAILED", e)));
                        }
                    },
                    Err(e) => {
                        let _ = reply.send(Err(BrowserError::new(
                            "SCREENSHOT_FAILED",
                            format!("webkit snapshot: {e}"),
                        )));
                    }
                }
            },
        );
        return;
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        let _ = (s, sid, tab, path, region);
        let _ = reply.send(Err(BrowserError::new(
            "UNSUPPORTED",
            "browser_screenshot is implemented on Linux (WebKitGTK) today; Windows/macOS capture lands next",
        )));
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn write_cairo_png(surface: &cairo::Surface, path: &str) -> Result<(u32, u32, usize), String> {
    use std::io::Write;
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let mut file = std::fs::File::create(path).map_err(|e| format!("create {path}: {e}"))?;
    surface
        .write_to_png(&mut file)
        .map_err(|e| format!("write_to_png: {e}"))?;
    file.flush().map_err(|e| format!("flush: {e}"))?;
    let meta = std::fs::metadata(path).map_err(|e| format!("stat: {e}"))?;
    let bytes = meta.len() as usize;
    let (width, height) = png_ihdr(path).unwrap_or((0, 0));
    Ok((width, height, bytes))
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn png_ihdr(path: &str) -> Option<(u32, u32)> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 24 || data[0] != 0x89 || &data[1..4] != b"PNG" {
        return None;
    }
    let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
    Some((w, h))
}

fn create_session(
    rt: &mut Runtime,
    elwt: &tao::event_loop::EventLoopWindowTarget<Cmd>,
    visible: bool,
    width: u32,
    height: u32,
) -> Result<Value, BrowserError> {
    let id = format!("browser_{:x}", ID_SEQ.fetch_add(1, Ordering::Relaxed));
    let tab_id = "tab_1".to_string();
    let window = WindowBuilder::new()
        .with_title("Catalyst Browser")
        .with_inner_size(LogicalSize::new(width as f64, height as f64))
        .with_visible(visible)
        .build(elwt)
        .map_err(|e| BrowserError::new("CREATE_FAILED", format!("window: {e}")))?;
    let webview = {
        let builder = WebViewBuilder::new().with_url("about:blank");
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            let vbox = window.default_vbox().ok_or_else(|| {
                BrowserError::new("CREATE_FAILED", "window has no default gtk::Box (vbox)")
            })?;
            builder
                .build_gtk(vbox)
                .map_err(|e| BrowserError::new("CREATE_FAILED", format!("webview gtk: {e}")))?
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            builder
                .build(&window)
                .map_err(|e| BrowserError::new("CREATE_FAILED", format!("webview: {e}")))?
        }
    };
    rt.sessions.insert(
        id.clone(),
        Session {
            id: id.clone(),
            tab_id: tab_id.clone(),
            visible,
            last_activity: Instant::now(),
            tab: TabState {
                window,
                webview,
                url: "about:blank".into(),
                title: String::new(),
            },
        },
    );
    Ok(create_response(&id, &tab_id, "ephemeral"))
}

fn close_session(rt: &mut Runtime, session_id: &str) -> Result<Value, BrowserError> {
    match rt.sessions.remove(session_id) {
        Some(s) => Ok(ok_envelope(&s.id, &s.tab_id, json!({ "closed": true }))),
        None => Err(BrowserError::new(
            "SESSION_NOT_FOUND",
            format!("no session {session_id}"),
        )),
    }
}

fn navigation_url_allowed(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http") | Some("https"))
        || url.eq_ignore_ascii_case("about:blank")
}

#[cfg(test)]
mod navigation_tests {
    use super::navigation_url_allowed;

    #[test]
    fn navigation_allows_only_web_and_blank() {
        assert!(navigation_url_allowed("https://example.com"));
        assert!(navigation_url_allowed("HTTP://example.com"));
        assert!(navigation_url_allowed("about:blank"));
        assert!(!navigation_url_allowed("file:///etc/passwd"));
        assert!(!navigation_url_allowed("javascript:alert(1)"));
        assert!(!navigation_url_allowed("data:text/html,secret"));
    }
}
fn navigate(rt: &mut Runtime, session_id: &str, url: &str) -> Result<Value, BrowserError> {
    if !navigation_url_allowed(url) {
        return Err(BrowserError::new(
            "NAVIGATE_BLOCKED",
            "browser navigation permits only http, https, and about:blank URLs",
        ));
    }
    let s = session_mut(rt, session_id)?;
    s.tab
        .webview
        .load_url(url)
        .map_err(|e| BrowserError::new("NAVIGATE_FAILED", format!("{e}")))?;
    s.tab.url = url.to_string();
    s.last_activity = Instant::now();
    Ok(ok_envelope(
        &s.id,
        &s.tab_id,
        json!({
            "url": url,
            "navigation": { "started": true, "completed": false },
            "snapshot_invalidated": true
        }),
    ))
}

fn reload(rt: &mut Runtime, session_id: &str) -> Result<Value, BrowserError> {
    let s = session_mut(rt, session_id)?;
    let url = s.tab.url.clone();
    if url.is_empty() {
        return Err(BrowserError::new("RELOAD_FAILED", "no url loaded"));
    }
    s.tab
        .webview
        .load_url(&url)
        .map_err(|e| BrowserError::new("RELOAD_FAILED", format!("{e}")))?;
    s.last_activity = Instant::now();
    Ok(ok_envelope(
        &s.id,
        &s.tab_id,
        json!({ "url": url, "snapshot_invalidated": true }),
    ))
}

fn set_visible(rt: &mut Runtime, session_id: &str, visible: bool) -> Result<Value, BrowserError> {
    let s = session_mut(rt, session_id)?;
    s.tab.window.set_visible(visible);
    s.visible = visible;
    s.last_activity = Instant::now();
    Ok(ok_envelope(&s.id, &s.tab_id, json!({ "visible": visible })))
}

fn session_mut<'a>(rt: &'a mut Runtime, session_id: &str) -> Result<&'a mut Session, BrowserError> {
    rt.sessions
        .get_mut(session_id)
        .ok_or_else(|| BrowserError::new("SESSION_NOT_FOUND", format!("no session {session_id}")))
}

async fn send(build: impl FnOnce(Reply) -> Cmd) -> Result<Value, BrowserError> {
    let proxy = ensure_runtime()?;
    let (tx, rx) = oneshot::channel();
    proxy
        .send_event(build(tx))
        .map_err(|_| BrowserError::new("RUNTIME_CLOSED", "browser event loop closed"))?;
    // Eval callbacks can take a moment on cold pages.
    match tokio::time::timeout(Duration::from_secs(60), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(BrowserError::new(
            "RUNTIME_CLOSED",
            "browser reply channel dropped",
        )),
        Err(_) => Err(BrowserError::new(
            "TIMEOUT",
            "browser tool timed out after 60s",
        )),
    }
}

async fn eval_value(session_id: &str, script: String) -> Result<Value, BrowserError> {
    send(|reply| Cmd::EvalJs {
        session_id: session_id.to_string(),
        script,
        reply,
    })
    .await
}

pub async fn dispatch(name: &str, args: &Value, cfg: &Config) -> Result<Value, BrowserError> {
    match name {
        "browser_create" => {
            let visible = args
                .get("visible")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let width = args
                .get("viewport")
                .and_then(|v| v.get("width"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1440) as u32;
            let height = args
                .get("viewport")
                .and_then(|v| v.get("height"))
                .and_then(|v| v.as_u64())
                .unwrap_or(900) as u32;
            send(|reply| Cmd::Create {
                visible,
                width,
                height,
                reply,
            })
            .await
        }
        "browser_close" => {
            let sid = require_session_id(args)?.to_string();
            send(|reply| Cmd::Close {
                session_id: sid,
                reply,
            })
            .await
        }
        "browser_list_sessions" => send(|reply| Cmd::List { reply }).await,
        "browser_navigate" => {
            let sid = require_session_id(args)?.to_string();
            let url = str_arg(args, "url")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "url is required"))?
                .to_string();
            let wait_until = args
                .get("wait_until")
                .and_then(|v| v.as_str())
                .unwrap_or("dom_stable");
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000)
                .min(60_000);
            let mut out = send(|reply| Cmd::Navigate {
                session_id: sid.clone(),
                url: url.clone(),
                reply,
            })
            .await?;
            if wait_until != "none" && wait_until != "navigation_started" {
                // Poll readyState / brief settle after navigation.
                let _ = wait_for_script(
                    &sid,
                    "document.readyState === 'interactive' || document.readyState === 'complete'"
                        .into(),
                    timeout_ms.min(15_000),
                    100,
                )
                .await;
                // Extra settle for SPA/DOM mutations.
                if wait_until == "dom_stable" || wait_until == "page_loaded" {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                if let Some(obj) = out.as_object_mut() {
                    obj.insert(
                        "navigation".into(),
                        json!({ "started": true, "completed": true }),
                    );
                }
            }
            Ok(out)
        }
        "browser_back" => {
            let sid = require_session_id(args)?.to_string();
            send(|reply| Cmd::Back {
                session_id: sid,
                reply,
            })
            .await
        }
        "browser_reload" => {
            let sid = require_session_id(args)?.to_string();
            send(|reply| Cmd::Reload {
                session_id: sid,
                reply,
            })
            .await
        }
        "browser_show" => {
            let sid = require_session_id(args)?.to_string();
            send(|reply| Cmd::SetVisible {
                session_id: sid,
                visible: true,
                reply,
            })
            .await
        }
        "browser_hide" => {
            let sid = require_session_id(args)?.to_string();
            send(|reply| Cmd::SetVisible {
                session_id: sid,
                visible: false,
                reply,
            })
            .await
        }
        "browser_evaluate" => {
            let sid = require_session_id(args)?.to_string();
            let script = str_arg(args, "script")
                .ok_or_else(|| BrowserError::new("INVALID_ARGS", "script is required"))?
                .to_string();
            // Allow bare expressions and statements that return.
            let value = eval_value(&sid, script).await?;
            Ok(ok_envelope(
                &sid,
                "tab_1",
                json!({
                    "success": true,
                    "value": value,
                    "value_type": type_name(&value)
                }),
            ))
        }
        "browser_snapshot" => snapshot(args).await,
        "browser_find" => find(args).await,
        "browser_click" => click_ref(args).await,
        "browser_fill" | "browser_type" => fill_ref(args).await,
        "browser_press" => press_key(args).await,
        "browser_scroll" => scroll_page(args).await,
        "browser_wait" => wait_cond(args).await,
        "browser_screenshot" => {
            let sid = require_session_id(args)?.to_string();
            let target = args
                .get("target")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("viewport");
            let region = match target {
                "full_page" | "full" | "window" => "full_page",
                _ => "viewport",
            }
            .to_string();
            let rel = args
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    format!(".catalyst-code/browser-screenshots/{sid}-{ts}.png")
                });
            let abs = crate::workspace::resolve(&cfg.workspace, &rel)
                .map_err(|e| BrowserError::new("INVALID_ARGS", format!("screenshot path: {e}")))?;
            if let Some(parent) = abs.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let path = abs.to_string_lossy().to_string();
            let out = send(|reply| Cmd::Screenshot {
                session_id: sid.clone(),
                path: path.clone(),
                region,
                reply,
            })
            .await?;
            let mut out = out;
            if let Some(obj) = out.as_object_mut() {
                obj.insert("path".into(), json!(rel));
                obj.insert("absolute_path".into(), json!(path));
            }
            Ok(out)
        }

        other => Err(BrowserError::new(
            "UNKNOWN_TOOL",
            format!("unhandled browser tool: {other}"),
        )),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

async fn snapshot(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let snap_id = next_snapshot_id();
    let script = include_str!("bridge/snapshot.js").to_string();
    let mut body = eval_value(&sid, script).await?;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("snapshot_id".into(), json!(snap_id));
    }
    Ok(ok_envelope(
        &sid,
        "tab_1",
        if let Value::Object(map) = body {
            Value::Object(map)
        } else {
            json!({ "snapshot_id": snap_id, "raw": body })
        },
    ))
}

async fn find(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let q = args
        .get("query")
        .ok_or_else(|| BrowserError::new("INVALID_ARGS", "query is required"))?;
    let strategy = q.get("strategy").and_then(|v| v.as_str()).unwrap_or("text");
    let value = q
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BrowserError::new("INVALID_ARGS", "query.value is required"))?;
    let max = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(20);
    let script = format!(
        "window.__cc_find_strategy = {}; window.__cc_find_value = {}; {}",
        serde_json::to_string(strategy).unwrap(),
        serde_json::to_string(value).unwrap(),
        include_str!("bridge/find.js")
    );
    let matches = eval_value(&sid, script).await?;
    let matches = match matches {
        Value::Array(a) => Value::Array(a.into_iter().take(max as usize).collect()),
        other => json!([other]),
    };
    Ok(json!({
        "success": true,
        "session_id": sid,
        "snapshot_id": next_snapshot_id(),
        "matches": matches
    }))
}

async fn click_ref(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let refr =
        str_arg(args, "ref").ok_or_else(|| BrowserError::new("INVALID_ARGS", "ref is required"))?;
    let v = serde_json::to_string(refr).unwrap();
    let script = format!(
        r#"(function(){{
  var el = document.querySelector('[data-catalyst-ref="' + {v} + '"]');
  if (!el) throw new Error('ELEMENT_STALE');
  el.scrollIntoView({{block:'center', inline:'center'}});
  el.click();
  return {{ ok: true, ref: {v} }};
}})()"#
    );
    let _ = eval_value(&sid, script).await?;
    Ok(ok_envelope(&sid, "tab_1", json!({ "clicked": refr })))
}

async fn fill_ref(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let refr =
        str_arg(args, "ref").ok_or_else(|| BrowserError::new("INVALID_ARGS", "ref is required"))?;
    let text = str_arg(args, "text").unwrap_or("");
    let r = serde_json::to_string(refr).unwrap();
    let t = serde_json::to_string(text).unwrap();
    let script = format!(
        r#"(function(){{
  var el = document.querySelector('[data-catalyst-ref="' + {r} + '"]');
  if (!el) throw new Error('ELEMENT_STALE');
  var val = {t};
  var proto = window.HTMLInputElement && HTMLInputElement.prototype;
  var d = proto && Object.getOwnPropertyDescriptor(proto, 'value');
  if (d && d.set) d.set.call(el, val); else el.value = val;
  el.dispatchEvent(new Event('input', {{bubbles:true}}));
  el.dispatchEvent(new Event('change', {{bubbles:true}}));
  return {{ ok: true, ref: {r} }};
}})()"#
    );
    let _ = eval_value(&sid, script).await?;
    Ok(ok_envelope(&sid, "tab_1", json!({ "filled": refr })))
}

async fn press_key(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let key =
        str_arg(args, "key").ok_or_else(|| BrowserError::new("INVALID_ARGS", "key is required"))?;
    let k = serde_json::to_string(key).unwrap();
    let script = if let Some(refr) = str_arg(args, "ref") {
        let r = serde_json::to_string(refr).unwrap();
        format!(
            r#"(function(){{
  var el = document.querySelector('[data-catalyst-ref="' + {r} + '"]') || document.activeElement || document.body;
  el.focus && el.focus();
  el.dispatchEvent(new KeyboardEvent('keydown', {{key:{k}, bubbles:true}}));
  el.dispatchEvent(new KeyboardEvent('keyup', {{key:{k}, bubbles:true}}));
  if ({k} === 'Enter' && el.form && el.form.requestSubmit) el.form.requestSubmit();
  return {{ ok: true, key: {k} }};
}})()"#
        )
    } else {
        format!(
            r#"(function(){{
  var el = document.activeElement || document.body;
  el.dispatchEvent(new KeyboardEvent('keydown', {{key:{k}, bubbles:true}}));
  el.dispatchEvent(new KeyboardEvent('keyup', {{key:{k}, bubbles:true}}));
  return {{ ok: true, key: {k} }};
}})()"#
        )
    };
    let _ = eval_value(&sid, script).await?;
    Ok(ok_envelope(&sid, "tab_1", json!({ "key": key })))
}

async fn scroll_page(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let dir = args
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("down");
    let amount = args.get("amount").and_then(|v| v.as_f64()).unwrap_or(700.0);
    let (dx, dy) = match dir {
        "up" => (0.0, -amount),
        "left" => (-amount, 0.0),
        "right" => (amount, 0.0),
        _ => (0.0, amount),
    };
    let script =
        format!("window.scrollBy({dx},{dy}); ({{ x: window.scrollX, y: window.scrollY }})");
    let pos = eval_value(&sid, script).await?;
    Ok(ok_envelope(
        &sid,
        "tab_1",
        json!({ "scrolled": { "dx": dx, "dy": dy }, "position": pos }),
    ))
}

fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Null => false,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty() && s != "false" && s != "0",
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.contains_key("__cc_error"),
    }
}

async fn wait_for_script(
    sid: &str,
    script: String,
    timeout_ms: u64,
    poll_ms: u64,
) -> Result<Value, BrowserError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last = Value::Null;
    loop {
        match eval_value(sid, script.clone()).await {
            Ok(v) => {
                last = v.clone();
                if js_truthy(&v) {
                    return Ok(v);
                }
            }
            Err(e) if e.code == "SESSION_NOT_FOUND" => return Err(e),
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::new(
                "WAIT_TIMEOUT",
                format!("timed out waiting; last={last}"),
            )
            .retryable());
        }
        tokio::time::sleep(Duration::from_millis(poll_ms.max(50))).await;
    }
}

async fn wait_cond(args: &Value) -> Result<Value, BrowserError> {
    let sid = require_session_id(args)?.to_string();
    let cond = args
        .get("condition")
        .ok_or_else(|| BrowserError::new("INVALID_ARGS", "condition is required"))?;
    let ctype = cond
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("timeout");
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(15_000)
        .min(60_000);
    let poll = args
        .get("poll_interval_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(100);

    if ctype == "timeout" {
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        return Ok(ok_envelope(&sid, "tab_1", json!({ "waited": "timeout" })));
    }

    let script = match ctype {
        "text" => {
            let v = cond.get("value").and_then(|x| x.as_str()).unwrap_or("");
            let vj = serde_json::to_string(v).unwrap();
            format!("(document.body && document.body.innerText || '').includes({vj})")
        }
        "text_gone" => {
            let v = cond.get("value").and_then(|x| x.as_str()).unwrap_or("");
            let vj = serde_json::to_string(v).unwrap();
            format!("!(document.body && document.body.innerText || '').includes({vj})")
        }
        "url" => {
            let v = cond.get("value").and_then(|x| x.as_str()).unwrap_or("");
            let vj = serde_json::to_string(v).unwrap();
            format!("location.href === {vj}")
        }
        "url_contains" => {
            let v = cond.get("value").and_then(|x| x.as_str()).unwrap_or("");
            let vj = serde_json::to_string(v).unwrap();
            format!("location.href.includes({vj})")
        }
        "title" => {
            let v = cond.get("value").and_then(|x| x.as_str()).unwrap_or("");
            let vj = serde_json::to_string(v).unwrap();
            format!("document.title.includes({vj})")
        }
        "dom_stable" | "page_loaded" | "dom_content_loaded" => {
            "document.readyState === 'complete'".into()
        }
        "javascript" => cond
            .get("script")
            .and_then(|x| x.as_str())
            .unwrap_or("true")
            .to_string(),
        "element" | "element_visible" => {
            let r = cond.get("ref").and_then(|x| x.as_str()).unwrap_or("");
            let rj = serde_json::to_string(r).unwrap();
            format!(
                "(function(){{ var el=document.querySelector('[data-catalyst-ref=' + JSON.stringify({rj}) + ']'); return !!el; }})()",
                rj = rj
            )
        }
        "element_gone" => {
            let r = cond.get("ref").and_then(|x| x.as_str()).unwrap_or("");
            let rj = serde_json::to_string(r).unwrap();
            format!(
                "(function(){{ var el=document.querySelector('[data-catalyst-ref=' + JSON.stringify({rj}) + ']'); return !el; }})()",
                rj = rj
            )
        }
        other => {
            return Err(BrowserError::new(
                "UNSUPPORTED",
                format!("wait condition type not implemented: {other}"),
            ));
        }
    };

    let _ = wait_for_script(&sid, script, timeout_ms, poll).await?;
    Ok(ok_envelope(
        &sid,
        "tab_1",
        json!({ "waited": ctype, "approximate": false }),
    ))
}
