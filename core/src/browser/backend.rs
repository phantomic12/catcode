//! Browser backend dispatch.
//!
//! Chromium/CDP is preferred when enabled; WRY/tao remains an explicit fallback.
//! Without either feature, tools return structured `BROWSER_UNAVAILABLE`.

use super::{ok_envelope, BrowserError};
use crate::config::Config;
use serde_json::{json, Value};

pub async fn dispatch(name: &str, args: &Value, cfg: &Config) -> Result<Value, BrowserError> {
    #[cfg(feature = "chromium-cdp")]
    {
        return crate::browser::chromium_cdp::dispatch(name, args, cfg).await;
    }
    #[cfg(all(not(feature = "chromium-cdp"), feature = "native-browser"))]
    {
        return crate::browser::wry_backend::dispatch(name, args, cfg).await;
    }
    #[cfg(all(not(feature = "chromium-cdp"), not(feature = "native-browser")))]
    {
        let _ = (args, cfg);
        if name == "browser_list_sessions" {
            return Ok(json!({ "success": true, "sessions": [] }));
        }
        Err(BrowserError::new(
            "BROWSER_UNAVAILABLE",
            "Browser support is not compiled into this core binary. Rebuild with `--features chromium-cdp` (preferred) or `--features native-browser` (WRY fallback).",
        ))
    }
}

#[allow(dead_code)]
pub(crate) fn require_session_id(args: &Value) -> Result<&str, BrowserError> {
    args.get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| BrowserError::new("INVALID_ARGS", "session_id is required"))
}

#[allow(dead_code)]
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

#[allow(dead_code)]
pub(crate) fn stub_capabilities() -> Value {
    json!({
        "screenshots": cfg!(feature = "native-browser"),
        "downloads": false,
        "file_uploads": false,
        "network_observation": "none",
        "native_accessibility": false,
        "visual_mode": cfg!(feature = "native-browser")
    })
}

#[allow(dead_code)]
pub(crate) fn create_response(session_id: &str, tab_id: &str, profile: &str) -> Value {
    #[allow(unused_mut)]
    let mut caps = stub_capabilities();
    #[allow(unused_mut)]
    let mut warnings: Vec<Value> = Vec::new();
    #[cfg(feature = "native-browser")]
    {
        if let Some(info) = crate::browser::headless_display::current_display() {
            caps["display"] = json!(info.display);
            caps["auto_xvfb"] = json!(info.auto_xvfb);
            if info.auto_xvfb {
                warnings.push(json!(
                    "No DISPLAY — started a private Xvfb for this browser runtime. Screenshots work; the window is not on a physical screen."
                ));
            }
        }
    }
    let mut extra = json!({
        "profile": { "type": profile },
        "capabilities": caps
    });
    if !warnings.is_empty() {
        extra["warnings"] = json!(warnings);
    }
    ok_envelope(session_id, tab_id, extra)
}
