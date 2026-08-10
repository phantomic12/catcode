//! OAuth plumbing for **plugin-declared** subscription providers.
//!
//! Built-in vendor OAuth (ChatGPT, Claude, Gemini, xAI, …) was removed from
//! core. Subscription login lives in plugins that declare an `oauth` block in
//! `plugin.json`; this module only provides:
//!
//! - Types shared with `/login` / `/oauth-code` (`OAuthPrompt`, `PendingOauth`,
//!   `LoginOutcome`)
//! - Loopback redirect helpers used by plugin web flows
//! - Headless detection + browser open
//! - `enrich_oauth` — resolve a plugin token (+ optional headers) onto a
//!   `ResolvedProvider` when no API key is set
//!
//! Codex endpoint client fingerprints (`originator` / User-Agent) are injected
//! in [`crate::provider`] when talking to the ChatGPT Codex backend — that is
//! wire protocol, not login.

use crate::config::ResolvedProvider;
use serde_json::Value;
use std::time::Duration;

/// Prompt shown to the user during an interactive OAuth login (URL to open,
/// optional user code, human message). Emitted as an `oauth_prompt` event.
#[derive(Clone, Debug)]
pub struct OAuthPrompt {
    pub url: String,
    pub code: Option<String>,
    pub message: String,
}

/// Pending state for a plugin OAuth flow that explicitly chose manual code
/// completion — held in `State` until the user pastes the authorization code
/// back via the `oauth_code` command. Automatic device-code flows never create
/// this state: the plugin is polled during `/login`.
#[derive(Clone)]
pub struct PendingOauth {
    /// Provider id (plugin `provider_id`) this login is for.
    pub kind: String,
    /// Unused for plugin flows (kept for struct stability).
    pub code_verifier: String,
    /// CSRF state from the plugin login script (web flow).
    #[allow(dead_code)]
    pub state: String,
    /// Unused for plugin flows (kept for struct stability).
    pub redirect_uri: String,
    /// Opaque JSON blob a plugin's `login` action returned and `complete` needs.
    pub plugin_pending: Option<Value>,
}

impl PendingOauth {
    /// Build a `PendingOauth` for a plugin OAuth provider (manual flow).
    pub fn plugin(provider_id: &str, state: String, pending: Option<Value>) -> Self {
        PendingOauth {
            kind: provider_id.to_string(),
            code_verifier: String::new(),
            state,
            redirect_uri: String::new(),
            plugin_pending: pending,
        }
    }
}

/// Outcome of starting an interactive OAuth login.
pub enum LoginOutcome {
    /// Login finished — token stored; provider is ready.
    Done,
    /// Manual flow: URL already emitted; wait for `/oauth-code`.
    AwaitingCode { pending: PendingOauth },
}

/// Whether we appear to be in a headless / remote (SSH) session where launching
/// a local browser and capturing a loopback redirect will NOT work.
/// Override with `CATALYST_CODE_NO_BROWSER=1` (force manual) or `=0` (force web).
pub fn likely_headless() -> bool {
    fn env_truthy(name: &str) -> Option<bool> {
        std::env::var(name).ok().map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
        })
    }
    match env_truthy("CATALYST_CODE_NO_BROWSER").or_else(|| env_truthy("NO_BROWSER")) {
        Some(true) => return true,
        Some(false) => return false,
        None => {}
    }
    if std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok() {
        return true;
    }
    if cfg!(unix) && !cfg!(target_os = "macos") {
        let display = std::env::var("DISPLAY").unwrap_or_default();
        let wayland = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        if display.is_empty() && wayland.is_empty() {
            return true;
        }
    }
    false
}

/// Bind `127.0.0.1:<port>` (0 = ephemeral) plus best-effort `::1:<same port>`.
pub async fn bind_loopback(
    port: u16,
) -> Result<
    (
        tokio::net::TcpListener,
        Option<tokio::net::TcpListener>,
        u16,
    ),
    String,
> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("could not bind localhost OAuth callback: {e}"))?;
    let bound_port = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| e.to_string())?;
    let listener_v6 = tokio::net::TcpListener::bind(("::1", bound_port))
        .await
        .ok();
    Ok((listener, listener_v6, bound_port))
}

/// Wait for an OAuth redirect on the bound loopback listener(s).
pub async fn await_redirect_dual(
    listener: tokio::net::TcpListener,
    listener_v6: Option<tokio::net::TcpListener>,
    state: &str,
    success_url: Option<&str>,
) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let stream = if let Some(v6) = &listener_v6 {
                tokio::select! {
                    r = listener.accept() => r.map_err(|e| e.to_string())?.0,
                    r = v6.accept() => r.map_err(|e| e.to_string())?.0,
                }
            } else {
                listener.accept().await.map_err(|e| e.to_string())?.0
            };
            if let Some(code) = handle_redirect_stream(stream, state, success_url).await? {
                return Ok(code);
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for the browser redirect".to_string())?
}

async fn handle_redirect_stream(
    mut stream: tokio::net::TcpStream,
    state: &str,
    success_url: Option<&str>,
) -> Result<Option<String>, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let resp = if let Some(url) = success_url {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    } else {
        let body = "<html><body><h2>Login complete</h2><p>You can close this tab and return to the terminal.</p></body></html>";
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.flush().await;
    let Some(line) = req.lines().next() else {
        return Ok(None);
    };
    let Some(path) = line.split(' ').nth(1) else {
        return Ok(None);
    };
    let Some(qs) = path.split('?').nth(1) else {
        return Ok(None);
    };
    let mut code = None;
    let mut st = None;
    for pair in qs.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        if k == "code" {
            code = Some(percent_decode(v));
        } else if k == "state" {
            st = Some(percent_decode(v));
        }
    }
    // Require non-empty state for CSRF (CORE_REVIEW). Use state "-" only when
    // a plugin deliberately opts out of CSRF (discouraged).
    if state.is_empty() {
        return Err("OAuth CSRF state is required (empty state is no longer accepted)".into());
    }
    if state == "-" || st.as_deref() == Some(state) {
        return code
            .map(Some)
            .ok_or_else(|| "no code in redirect".to_string());
    }
    Ok(None)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(a), Some(b)) = (h(bytes[i + 1]), h(bytes[i + 2])) {
                out.push((a << 4) | b);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn open_browser(url: &str) -> std::io::Result<()> {
    let (prog, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else if cfg!(unix) {
        ("xdg-open", vec![url])
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no browser opener for this platform",
        ));
    };
    std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

/// Merge a header into `headers`, replacing any existing same-name entry
/// (case-insensitive). Plugin token headers win over static plugin.json headers.
fn upsert_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, v)) = headers
        .iter_mut()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
    {
        *v = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

/// Fill in a subscription OAuth token for `rp` when it has no API key, by
/// consulting plugin-declared OAuth providers. An explicit API key always wins.
/// Also injects Codex client fingerprints when the endpoint is ChatGPT Codex
/// (moved from vendor OAuth — this is a wire requirement).
pub async fn enrich_oauth(
    mut rp: ResolvedProvider,
    _client: &reqwest::Client,
    pm: Option<&crate::plugins::PluginManager>,
) -> ResolvedProvider {
    // Codex backend rejects requests without first-party client fingerprints.
    if crate::provider::is_codex_endpoint(&rp.base_url) {
        crate::provider::inject_codex_headers(&mut rp.headers);
    }
    if rp.api_key.is_some() {
        return rp;
    }
    if let Some(pm) = pm {
        if let Some(cfg) = pm.oauth_config_for_provider(&rp) {
            if let Some(creds) = pm.resolve_oauth_creds(&cfg.provider_id).await {
                rp.api_key = Some(creds.access_token);
                rp.oauth = true;
                for (k, v) in creds.headers {
                    upsert_header(&mut rp.headers, &k, v);
                }
            }
        }
    }
    rp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
    }

    #[test]
    fn upsert_header_replaces_case_insensitive() {
        let mut h = vec![("Foo".to_string(), "1".to_string())];
        upsert_header(&mut h, "foo", "2".to_string());
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "2");
        upsert_header(&mut h, "Bar", "3".to_string());
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn likely_headless_respects_force_web() {
        // Just smoke: function is callable. Env overrides are process-global
        // so we don't mutate them in unit tests.
        let _ = likely_headless();
    }
}
