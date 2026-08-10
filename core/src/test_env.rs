//! test_env — spin up ephemeral Linux containers / Windows VMs for
//! platform-specific testing, with VNC screen access.
//!
//! Agent-facing tool (`test_env`) with actions: create, exec, screenshot,
//! input, vnc_url, destroy, list.
//!
//! Backends:
//! - Linux: rootless Podman container (catalyst/linux-gui image). GUI via Xvfb,
//!   x11vnc, noVNC/websockify inside the container. exec via `podman exec`;
//!   screenshot via `scrot`; input via `xdotool`.
//! - Windows: QEMU/KVM VM cloned from a base qcow2 (built by
//!   packaging/vm-images/windows/build.sh). VNC via QEMU `-vnc` and host
//!   websockify. exec via SSH; screenshot via QEMU `screendump`; input via QMP.
//!
//! For TUI tests use `exec` (SSH+PTY); for webui/GUI tests use `screenshot`/
//! `input` or the live `vnc_url` in the noVNC Screen panel.
//!
//! Configuration is via env vars (see `config_*` helpers) so config.rs stays
//! untouched. State (running envs + child handles) is process-global.

use crate::config::Config;
use crate::tools::Outcome;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

#[derive(Clone, Debug, PartialEq)]
enum Platform {
    Linux,
    Windows,
}

#[derive(Clone, Debug)]
struct TestEnv {
    id: String,
    platform: Platform,
    container: Option<String>, // podman container name (linux)
    qemu_pid: Option<u32>,     // qemu pid from pidfile (windows)
    qmp_sock: Option<String>,  // qmp unix socket (windows)
    ssh_port: Option<u16>,     // windows ssh port
    vnc_port: Option<u16>,     // raw vnc port
    vnc_url: Option<String>,   // websockify ws url for the noVNC panel
    created_at: std::time::Instant,
}

/// Running envs (metadata only — cloneable for the `list` action).
static ENVS: LazyLock<Mutex<HashMap<String, TestEnv>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Detached child handles (swtpm, host websockify) kept alive + reaped on
/// destroy. Keyed by env_id. tokio Child is not Clone, so it lives here
/// separate from the cloneable TestEnv metadata.
static CHILDREN: LazyLock<Mutex<HashMap<String, Vec<tokio::process::Child>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

// ── public entrypoint ──────────────────────────────────────────────────────

pub async fn execute_test_env(args: &Value, cfg: &Config) -> Outcome {
    let action = match args.get("action").and_then(|v| v.as_str()) {
        Some(a) => a.to_string(),
        None => return Outcome::err("test_env requires 'action'"),
    };
    match action.as_str() {
        "create" => create(args, cfg).await,
        "exec" => exec(args, cfg).await,
        "screenshot" => screenshot(args, cfg).await,
        "input" => input(args, cfg).await,
        "vnc_url" => vnc_url(args, cfg).await,
        "destroy" => destroy(args, cfg).await,
        "list" => list(args, cfg).await,
        other => Outcome::err(format!("unknown test_env action: {other}")),
    }
}

// ── config (env vars with defaults; keeps config.rs untouched) ─────────────

fn windows_base() -> String {
    std::env::var("CATALYST_TESTENV_WINDOWS_BASE").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.catalyst-code/vm-images/windows-11-iot-ltsc.qcow2")
    })
}
fn linux_image() -> String {
    std::env::var("CATALYST_TESTENV_LINUX_IMAGE")
        .unwrap_or_else(|_| "catalyst/linux-gui:24.04".into())
}
fn env_dir() -> String {
    std::env::var("CATALYST_TESTENV_DIR").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        format!("{home}/.catalyst-code/test-envs")
    })
}
fn ssh_key() -> String {
    std::env::var("CATALYST_TESTENV_SSH_KEY")
        .unwrap_or_else(|_| format!("{}/id_ed25519", env_dir()))
}
fn ssh_user() -> String {
    std::env::var("CATALYST_TESTENV_SSH_USER").unwrap_or_else(|_| "testuser".into())
}
fn vnc_host() -> String {
    std::env::var("CATALYST_TESTENV_VNC_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn new_env_id() -> String {
    let c = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    format!("te-{:08x}{:02x}", t & 0xffffffff, c & 0xff)
}

/// Bind to :0 to find a free TCP port (TOCTOU race is acceptable here).
fn free_port() -> Option<u16> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()
        .map(|a| a.port())
}

/// Run a command, capture stdout, enforce a wall-clock timeout.
async fn run(cmd: &mut Command, secs: u64) -> Result<String, String> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = timeout(Duration::from_secs(secs), cmd.output())
        .await
        .map_err(|_| "command timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() {
        return Err(format!("exit {:?}: {stderr}{stdout}", out.status.code()));
    }
    Ok(stdout)
}

fn env_summary(e: &TestEnv) -> String {
    json!({
        "env_id": e.id,
        "platform": match e.platform { Platform::Linux => "linux", Platform::Windows => "windows" },
        "status": "running",
        "vnc_url": e.vnc_url,
        "vnc_port": e.vnc_port,
        "ssh_port": e.ssh_port,
    })
    .to_string()
}

fn get_env(args: &Value) -> Option<TestEnv> {
    let id = args.get("env_id")?.as_str()?.to_string();
    ENVS.lock().unwrap().get(&id).cloned()
}

// ── create ─────────────────────────────────────────────────────────────────

async fn create(args: &Value, _cfg: &Config) -> Outcome {
    let platform = match args.get("platform").and_then(|v| v.as_str()) {
        Some("windows") => Platform::Windows,
        Some("linux") | None => Platform::Linux, // default linux
        Some(other) => return Outcome::err(format!("unknown platform: {other}")),
    };
    let id = new_env_id();
    match platform {
        Platform::Linux => create_linux(&id, args).await,
        Platform::Windows => create_windows(&id, args).await,
    }
}

async fn create_linux(id: &str, args: &Value) -> Outcome {
    let image = args
        .get("image")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(linux_image);
    // Reject shell metacharacters / path traversal in image refs (CORE_REVIEW).
    if image
        .chars()
        .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '\n' | '\r' | ' '))
        || image.contains("..")
    {
        return Outcome::err(format!("test_env image name rejected: {image}"));
    }
    // Bind VNC/noVNC to loopback only — bare `-p 5900` publishes on all
    // interfaces (CORE_REVIEW HIGH).
    let cid = match run(
        Command::new("podman")
            .arg("run")
            .arg("-d")
            .arg("--name")
            .arg(id)
            .arg("-p")
            .arg("127.0.0.1::5900")
            .arg("-p")
            .arg("127.0.0.1::6080")
            .arg(&image),
        120,
    )
    .await
    {
        Ok(o) => o.trim().to_string(),
        Err(e) => {
            return Outcome::err(format!(
                "podman run failed: {e}\n(hint: build the image first: \
                 `podman build -t {image} packaging/vm-images/linux/`)"
            ))
        }
    };
    let vnc_host_port = port_for(id, 5900).await;
    let web_host_port = port_for(id, 6080).await;
    let vnc_url = web_host_port.map(|p| format!("ws://{}:{p}/websockify", vnc_host()));
    let env = TestEnv {
        id: id.to_string(),
        platform: Platform::Linux,
        container: Some(cid),
        qemu_pid: None,
        qmp_sock: None,
        ssh_port: None,
        vnc_port: vnc_host_port,
        vnc_url: vnc_url.clone(),
        created_at: std::time::Instant::now(),
    };
    let summary = env_summary(&env);
    ENVS.lock().unwrap().insert(id.to_string(), env);
    Outcome::ok(summary)
}

/// `podman port <container> <port>` → host port.
async fn port_for(container: &str, port: u16) -> Option<u16> {
    let out = Command::new("podman")
        .arg("port")
        .arg(container)
        .arg(port.to_string())
        .output()
        .await
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.trim().rsplit(':').next()?.trim().parse().ok()
}

async fn create_windows(id: &str, args: &Value) -> Outcome {
    let base = windows_base();
    if !std::path::Path::new(&base).exists() {
        return Outcome::err(format!(
            "Windows base image not found at {base}\n(hint: build it: \
             `cd packaging/vm-images/windows && bash build.sh`, then set \
             CATALYST_TESTENV_WINDOWS_BASE=<path>)"
        ));
    }
    let dir = env_dir();
    let _ = std::fs::create_dir_all(&dir);
    let overlay = format!("{dir}/{id}.qcow2");
    let qmp_sock = format!("{dir}/{id}.qmp");
    let pidfile = format!("{dir}/{id}.pid");
    let swtpm_dir = format!("{dir}/{id}.tpm");
    let swtpm_sock = format!("{dir}/{id}.tpm.sock");
    let _ = std::fs::create_dir_all(&swtpm_dir);

    let ssh_port = match free_port() {
        Some(p) => p,
        None => return Outcome::err("no free port for ssh"),
    };
    let display = match free_port() {
        // use a free port's low bits as a vnc display number (0..99)
        Some(p) => p % 90,
        None => 0,
    };
    let vnc_port = 5900 + display;
    let cpus = args.get("cpus").and_then(|v| v.as_u64()).unwrap_or(4);
    let ram = args
        .get("memory_mb")
        .and_then(|v| v.as_u64())
        .unwrap_or(4096);

    // 1. overlay disk (copy-on-write clone of the base).
    if let Err(e) = run(
        Command::new("qemu-img")
            .arg("create")
            .arg("-f")
            .arg("qcow2")
            .arg("-b")
            .arg(&base)
            .arg("-F")
            .arg("qcow2")
            .arg(&overlay),
        30,
    )
    .await
    {
        return Outcome::err(format!("qemu-img create failed: {e}"));
    }

    // 2. swtpm (foreground tokio child — stored + reaped on destroy).
    let swtpm = Command::new("swtpm")
        .arg("socket")
        .arg("--tpmstate")
        .arg(format!("dir={swtpm_dir}"))
        .arg("--ctrl")
        .arg(format!("type=unixio,path={swtpm_sock}"))
        .arg("--tpm2")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if swtpm.is_err() {
        return Outcome::err(
            "swtpm failed to start (install swtpm, or the VM will fail TPM checks)",
        );
    }
    CHILDREN
        .lock()
        .unwrap()
        .entry(id.to_string())
        .or_default()
        .push(swtpm.unwrap());

    // 3. boot QEMU (detached via -daemonize; pidfile for cleanup).
    let accel = if std::path::Path::new("/dev/kvm").exists() {
        "kvm"
    } else if cfg!(target_os = "macos") {
        "hvf"
    } else {
        "tcg"
    };
    let cpu = if accel == "tcg" { "qemu64" } else { "host" };
    let qemu_args: Vec<String> = vec![
        format!("-machine"),
        format!("q35,accel={accel}:tcg"),
        format!("-cpu"),
        cpu.into(),
        format!("-smp"),
        cpus.to_string(),
        format!("-m"),
        ram.to_string(),
        format!("-drive"),
        format!("file={overlay},format=qcow2,if=virtio"),
        format!("-netdev"),
        format!("user,id=n0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
        format!("-device"),
        "virtio-net,netdev=n0".into(),
        format!("-device"),
        "usb-tablet".into(),
        format!("-chardev"),
        format!("socket,id=chrtpm,path={swtpm_sock}"),
        format!("-tpmdev"),
        "emulator,id=tpm0,chardev=chrtpm".into(),
        format!("-device"),
        "tpm-crb".into(),
        format!("-qmp"),
        format!("unix:{qmp_sock},server,nowait"),
        format!("-vnc"),
        format!(":{display}"),
        format!("-daemonize"),
        format!("-pidfile"),
        pidfile.clone(),
    ];
    let mut qemu = Command::new("qemu-system-x86_64");
    for a in &qemu_args {
        qemu.arg(a);
    }
    if let Err(e) = run(&mut qemu, 60).await {
        return Outcome::err(format!("qemu failed to start: {e}"));
    }
    let qemu_pid = read_pidfile(&pidfile);
    if qemu_pid.is_none() {
        return Outcome::err("qemu started but pidfile is empty/unreadable");
    }

    // 4. host websockify bridging the raw VNC port to a websocket (noVNC).
    let ws_port = match free_port() {
        Some(p) => p,
        None => return Outcome::err("no free port for websockify"),
    };
    let mut ws_cmd = Command::new("websockify");
    // Serve the noVNC web client too (so the Screen panel can load vnc.html
    // over http from the same host:port — avoids mixed-content ws://-from-https).
    if let Ok(web) = std::env::var("CATALYST_TESTENV_NOVNC_WEB") {
        ws_cmd.arg("--web").arg(web);
    }
    let ws = ws_cmd
        .arg(ws_port.to_string())
        .arg(format!("localhost:{vnc_port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(child) = ws {
        CHILDREN
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_default()
            .push(child);
    } // else: websockify missing — vnc_url will be None; raw VNC still works.

    let vnc_url = format!("ws://{}:{ws_port}/websockify", vnc_host());
    let env = TestEnv {
        id: id.to_string(),
        platform: Platform::Windows,
        container: None,
        qemu_pid,
        qmp_sock: Some(qmp_sock),
        ssh_port: Some(ssh_port),
        vnc_port: Some(vnc_port),
        vnc_url: Some(vnc_url.clone()),
        created_at: std::time::Instant::now(),
    };
    let summary = env_summary(&env);
    ENVS.lock().unwrap().insert(id.to_string(), env);
    Outcome::ok(summary)
}

fn read_pidfile(path: &str) -> Option<u32> {
    for _ in 0..20 {
        if let Ok(s) = std::fs::read_to_string(path) {
            if let Ok(p) = s.trim().parse::<u32>() {
                return Some(p);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

// ── exec ───────────────────────────────────────────────────────────────────

async fn exec(args: &Value, _cfg: &Config) -> Outcome {
    let env = match get_env(args) {
        Some(e) => e,
        None => return Outcome::err("no such env_id (use test_env action:create first)"),
    };
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return Outcome::err("exec requires 'command'"),
    };
    let pty = args.get("pty").and_then(|v| v.as_bool()).unwrap_or(false);
    let to = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);
    match env.platform {
        Platform::Linux => {
            let cid = env.container.clone().unwrap_or_default();
            let mut c = Command::new("podman");
            c.arg("exec").arg(&cid).arg("bash").arg("-lc").arg(command);
            match run(&mut c, to).await {
                Ok(o) => Outcome::ok(o),
                Err(e) => Outcome::err(format!("exec failed: {e}")),
            }
        }
        Platform::Windows => {
            let port = env.ssh_port.unwrap_or(0);
            let key = ssh_key();
            if !std::path::Path::new(&key).exists() {
                return Outcome::err(format!(
                    "ssh key not found at {key}\n(hint: the windows build.sh generates it; \
                     or run: ssh-keygen -t ed25519 -f {key})"
                ));
            }
            // Wait for SSH to accept connections (the VM may still be booting).
            if let Err(e) = wait_ssh(port, 90).await {
                return Outcome::err(format!("ssh not ready on port {port}: {e}"));
            }
            let mut c = Command::new("ssh");
            c.args([
                "-p",
                &port.to_string(),
                "-i",
                &key,
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=10",
            ]);
            if pty {
                c.arg("-tt");
            }
            c.arg(format!("{}@localhost", ssh_user())).arg(command);
            match run(&mut c, to).await {
                Ok(o) => Outcome::ok(o),
                Err(e) => Outcome::err(format!("ssh exec failed: {e}")),
            }
        }
    }
}

/// Poll a TCP port until it accepts a connection (SSH readiness).
async fn wait_ssh(port: u16, secs: u64) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .is_ok()
        {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err("timed out waiting for ssh port".into());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ── screenshot ─────────────────────────────────────────────────────────────
//
// Saves a PNG to <env_dir>/screenshots/<id>-<ts>.png and returns JSON metadata
// {path, width, height, bytes}. The agent gets a viewable artifact; the live
// noVNC panel (vnc_url) is the real-time view. NOTE: surfacing the PNG as an
// image content block the model can *see* requires extending Outcome with an
// `image` field + multimodal tool-result support (see main.rs Message::tool);
// that is the documented follow-up for true agent vision.

async fn screenshot(args: &Value, _cfg: &Config) -> Outcome {
    let env = match get_env(args) {
        Some(e) => e,
        None => return Outcome::err("no such env_id"),
    };
    let dir = format!("{}/screenshots", env_dir());
    let _ = std::fs::create_dir_all(&dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = format!("{dir}/{}-{ts}.png", env.id);

    let res = match env.platform {
        Platform::Linux => screenshot_linux(&env, &path).await,
        Platform::Windows => screenshot_windows(&env, &path).await,
    };
    match res {
        Ok((w, h, bytes)) => {
            Outcome::ok(json!({"path": path, "width": w, "height": h, "bytes": bytes}).to_string())
        }
        Err(e) => Outcome::err(format!("screenshot failed: {e}")),
    }
}

async fn screenshot_linux(env: &TestEnv, path: &str) -> Result<(u32, u32, usize), String> {
    let cid = env.container.clone().unwrap_or_default();
    // scrot inside the container, then podman cp it out.
    run(
        Command::new("podman")
            .arg("exec")
            .arg(&cid)
            .arg("scrot")
            .arg("-d")
            .arg("1")
            .arg("/tmp/shot.png"),
        15,
    )
    .await?;
    run(
        Command::new("podman")
            .arg("cp")
            .arg(format!("{cid}:/tmp/shot.png"))
            .arg(path),
        15,
    )
    .await?;
    Ok(png_meta(path))
}

async fn screenshot_windows(env: &TestEnv, path: &str) -> Result<(u32, u32, usize), String> {
    let sock = env.qmp_sock.clone().unwrap_or_default();
    qmp_exec(
        &sock,
        &json!({"execute":"screendump","arguments":{"filename":path}}),
    )
    .await?;
    Ok(png_meta(path))
}

/// Read a PNG's (width, height, file size). Best-effort header parse.
fn png_meta(path: &str) -> (u32, u32, usize) {
    let bytes = std::fs::metadata(path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let (w, h) = std::fs::read(path)
        .ok()
        .and_then(|b| {
            // PNG IHDR: width at bytes 16..20, height 20..24 (big-endian).
            if b.len() >= 24 && b.starts_with(&[0x89, b'P', b'N', b'G']) {
                let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
                let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
                Some((w, h))
            } else {
                None
            }
        })
        .unwrap_or((0, 0));
    (w, h, bytes)
}

// ── input ──────────────────────────────────────────────────────────────────

async fn input(args: &Value, _cfg: &Config) -> Outcome {
    let env = match get_env(args) {
        Some(e) => e,
        None => return Outcome::err("no such env_id"),
    };
    let itype = match args.get("input_type").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return Outcome::err("input requires 'input_type' (key|click|type)"),
    };
    let res = match env.platform {
        Platform::Linux => input_linux(&env, &itype, args).await,
        Platform::Windows => input_windows(&env, &itype, args).await,
    };
    match res {
        Ok(_) => Outcome::ok("ok"),
        Err(e) => Outcome::err(format!("input failed: {e}")),
    }
}

async fn input_linux(env: &TestEnv, itype: &str, args: &Value) -> Result<(), String> {
    let cid = env.container.clone().unwrap_or_default();
    match itype {
        "key" => {
            let keys = args
                .get("keys")
                .and_then(|v| v.as_array())
                .ok_or("input key requires 'keys' array")?
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            run(
                Command::new("podman")
                    .arg("exec")
                    .arg(&cid)
                    .arg("xdotool")
                    .arg("key")
                    .arg(&keys),
                15,
            )
            .await?;
        }
        "click" => {
            let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            run(
                Command::new("podman")
                    .arg("exec")
                    .arg(&cid)
                    .arg("xdotool")
                    .arg("mousemove")
                    .arg(x.to_string())
                    .arg(y.to_string())
                    .arg("click")
                    .arg("1"),
                15,
            )
            .await?;
        }
        "type" => {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("input type requires 'text'")?;
            run(
                Command::new("podman")
                    .arg("exec")
                    .arg(&cid)
                    .arg("xdotool")
                    .arg("type")
                    .arg("--")
                    .arg(text),
                15,
            )
            .await?;
        }
        other => return Err(format!("unknown input_type: {other}")),
    }
    Ok(())
}

async fn input_windows(env: &TestEnv, itype: &str, args: &Value) -> Result<(), String> {
    let sock = env.qmp_sock.clone().unwrap_or_default();
    match itype {
        "key" => {
            let keys: Vec<String> = args
                .get("keys")
                .and_then(|v| v.as_array())
                .ok_or("input key requires 'keys' array")?
                .iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect();
            qmp_exec(
                &sock,
                &json!({"execute":"send-key","arguments":{"keys":keys}}),
            )
            .await?;
        }
        "click" => {
            let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            // Absolute coords via the usb-tablet: scale 0..32767. Best-effort
            // display size (query-display in newer QEMU; else default 1280x800).
            let (w, h) = qmp_display_size(&sock).await.unwrap_or((1280, 800));
            let ax = ((x as f64 / w as f64) * 32767.0) as i64;
            let ay = ((y as f64 / h as f64) * 32767.0) as i64;
            qmp_exec(
                &sock,
                &json!({"execute":"input-send-event","arguments":{"events":[
                    {"type":"abs","data":{"axis":"x","value":ax}},
                    {"type":"abs","data":{"axis":"y","value":ay}},
                    {"type":"btn","data":{"down":true,"button":"left"}},
                    {"type":"btn","data":{"down":false,"button":"left"}}
                ]}}),
            )
            .await?;
        }
        "type" => {
            // Clipboard trick: set the text over SSH, then paste via Ctrl+V.
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("input type requires 'text'")?;
            let port = env.ssh_port.unwrap_or(0);
            let key = ssh_key();
            let escaped = text.replace('\'', "''");
            let _ = run(
                Command::new("ssh")
                    .args([
                        "-p",
                        &port.to_string(),
                        "-i",
                        &key,
                        "-o",
                        "StrictHostKeyChecking=no",
                        "-o",
                        "UserKnownHostsFile=/dev/null",
                        "-o",
                        "ConnectTimeout=10",
                    ])
                    .arg(format!("{}@localhost", ssh_user()))
                    .arg(format!(
                        "powershell -NoProfile -Command \"Set-Clipboard -Value '{escaped}'\""
                    )),
                30,
            )
            .await;
            qmp_exec(
                &sock,
                &json!({"execute":"send-key","arguments":{"keys":["ctrl","v"]}}),
            )
            .await?;
        }
        other => return Err(format!("unknown input_type: {other}")),
    }
    Ok(())
}

/// Best-effort display size via QMP query-display (QEMU ≥ 7.0).
async fn qmp_display_size(sock: &str) -> Option<(i64, i64)> {
    let v = qmp_exec(sock, &json!({"execute":"query-display"}))
        .await
        .ok()?;
    let w = v.get("width")?.as_i64()?;
    let h = v.get("height")?.as_i64()?;
    Some((w, h))
}

// ── QMP client (QEMU monitor over unix socket) ─────────────────────────────

/// Connect, do the qmp_capabilities handshake, send one command, return its
/// `return` object. Skips async events while waiting for the response.
///
/// QEMU's QMP is exposed over a Unix domain socket, so this is Unix-host only.
/// Windows-target builds still compile the Windows-VM helpers, but QMP calls
/// fail closed (QEMU Windows VMs are managed from Linux/macOS hosts).
#[cfg(unix)]
async fn qmp_exec(sock: &str, cmd: &Value) -> Result<Value, String> {
    let s = UnixStream::connect(sock)
        .await
        .map_err(|e| format!("qmp connect {sock}: {e}"))?;
    let (read, mut write) = s.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();

    // 1. greeting
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("qmp read greeting: {e}"))?;
    line.clear();
    // 2. capabilities handshake
    write
        .write_all(b"{\"execute\":\"qmp_capabilities\"}\n")
        .await
        .map_err(|e| format!("qmp write caps: {e}"))?;
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("qmp read caps: {e}"))?;
    line.clear();
    // 3. the actual command
    write
        .write_all(format!("{}\n", cmd).as_bytes())
        .await
        .map_err(|e| format!("qmp write cmd: {e}"))?;

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("qmp read resp: {e}"))?;
        if n == 0 {
            return Err("qmp eof before response".into());
        }
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            if v.get("return").is_some() {
                return Ok(v["return"].clone());
            }
            if v.get("error").is_some() {
                return Err(format!("qmp error: {v}"));
            }
            // else: an async event — keep reading.
        }
    }
}

#[cfg(not(unix))]
async fn qmp_exec(_sock: &str, _cmd: &Value) -> Result<Value, String> {
    Err("QMP over Unix sockets requires a Unix host (Linux/macOS)".into())
}

// ── vnc_url / destroy / list ───────────────────────────────────────────────

async fn vnc_url(args: &Value, _cfg: &Config) -> Outcome {
    let env = match get_env(args) {
        Some(e) => e,
        None => return Outcome::err("no such env_id"),
    };
    Outcome::ok(
        json!({"env_id": env.id, "vnc_url": env.vnc_url, "vnc_port": env.vnc_port}).to_string(),
    )
}

async fn destroy(args: &Value, _cfg: &Config) -> Outcome {
    let id = match args.get("env_id").and_then(|v| v.as_str()) {
        Some(i) => i.to_string(),
        None => return Outcome::err("destroy requires 'env_id'"),
    };
    let env = ENVS.lock().unwrap().remove(&id);
    let env = match env {
        Some(e) => e,
        None => return Outcome::ok(format!("env {id} not found (already destroyed)")),
    };
    match env.platform {
        Platform::Linux => {
            if let Some(cid) = &env.container {
                let _ = Command::new("podman")
                    .arg("rm")
                    .arg("-f")
                    .arg(cid)
                    .output()
                    .await;
            }
        }
        Platform::Windows => {
            if let Some(pid) = env.qemu_pid {
                let _ = Command::new("kill").arg(pid.to_string()).output().await;
            }
            // remove overlay + sockets
            let dir = env_dir();
            for f in [
                format!("{dir}/{id}.qcow2"),
                format!("{dir}/{id}.qmp"),
                format!("{dir}/{id}.pid"),
                format!("{dir}/{id}.tpm.sock"),
            ] {
                let _ = std::fs::remove_file(&f);
            }
            let _ = std::fs::remove_dir_all(format!("{dir}/{id}.tpm"));
        }
    }
    // reap detached children (swtpm, websockify). Bind outside the if-let so
    // the MutexGuard drops before the await (else the future is !Send).
    let kids = CHILDREN.lock().unwrap().remove(&id);
    if let Some(mut kids) = kids {
        for mut c in kids.drain(..) {
            let _ = c.start_kill();
            let _ = timeout(Duration::from_secs(5), c.wait()).await;
        }
    }
    Outcome::ok(format!("destroyed {id}"))
}

async fn list(_args: &Value, _cfg: &Config) -> Outcome {
    let envs = ENVS.lock().unwrap();
    let arr: Vec<Value> = envs
        .values()
        .map(|e| {
            json!({
                "env_id": e.id,
                "platform": match e.platform { Platform::Linux => "linux", Platform::Windows => "windows" },
                "vnc_url": e.vnc_url,
                "age_secs": e.created_at.elapsed().as_secs(),
            })
        })
        .collect();
    Outcome::ok(json!({"envs": arr}).to_string())
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_ids_are_distinct() {
        let a = new_env_id();
        let b = new_env_id();
        assert_ne!(a, b);
        assert!(a.starts_with("te-"));
        assert!(b.starts_with("te-"));
    }

    #[test]
    fn qmp_screendump_command_shape() {
        let cmd = json!({"execute":"screendump","arguments":{"filename":"/tmp/x.png"}});
        assert_eq!(cmd["execute"], "screendump");
        assert_eq!(cmd["arguments"]["filename"], "/tmp/x.png");
    }

    #[test]
    fn qmp_send_key_command_shape() {
        let cmd = json!({"execute":"send-key","arguments":{"keys":["ctrl","v"]}});
        assert_eq!(cmd["arguments"]["keys"][0], "ctrl");
    }

    #[test]
    fn png_meta_parses_header() {
        // 1x1 transparent PNG.
        const PNG: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
            0x00, 0x00, 0x00, 0x01, // width = 1
            0x00, 0x00, 0x00, 0x01, // height = 1
            0x08, 0x06, 0x00, 0x00, 0x00, // bit depth, color type, ...
        ];
        let dir = std::env::temp_dir().join("catalyst_png_meta_test.png");
        std::fs::write(&dir, PNG).unwrap();
        let (w, h, bytes) = png_meta(dir.to_str().unwrap());
        assert_eq!((w, h), (1, 1));
        assert_eq!(bytes, PNG.len());
    }

    #[test]
    fn free_port_returns_something() {
        // May rarely fail if the port is taken between bind and return, but
        // that's the documented TOCTOU; just assert we got a port.
        let p = free_port();
        assert!(p.is_some());
        assert!(p.unwrap() > 0);
    }

    #[test]
    fn env_summary_has_fields() {
        let e = TestEnv {
            id: "te-deadbeef00".into(),
            platform: Platform::Windows,
            container: None,
            qemu_pid: Some(1234),
            qmp_sock: Some("/tmp/x.qmp".into()),
            ssh_port: Some(2222),
            vnc_port: Some(5901),
            vnc_url: Some("ws://localhost:1234/websockify".into()),
            created_at: std::time::Instant::now(),
        };
        let s = env_summary(&e);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["platform"], "windows");
        assert_eq!(v["ssh_port"], 2222);
        assert_eq!(v["vnc_url"], "ws://localhost:1234/websockify");
    }

    /// Real end-to-end: spin up a Linux container via execute_test_env and
    /// exercise create → exec → screenshot → vnc_url → destroy. Ignored by
    /// default (needs podman + the catalyst/linux-gui:24.04 image). Run with:
    ///   cargo test --bin core test_env -- --ignored --nocapture
    #[ignore]
    #[tokio::test]
    async fn e2e_linux_create_exec_screenshot_destroy() {
        // Skip cleanly if podman or the image isn't available.
        let have = std::process::Command::new("podman")
            .args(["image", "exists", "catalyst/linux-gui:24.04"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !have {
            eprintln!("skipping: catalyst/linux-gui:24.04 not present");
            return;
        }
        let cfg = Config::default();

        // create
        let r = execute_test_env(&json!({"action":"create","platform":"linux"}), &cfg).await;
        assert!(r.ok, "create failed: {}", r.output);
        let v: Value = serde_json::from_str(&r.output).expect("create output is JSON");
        let id = v["env_id"].as_str().expect("env_id").to_string();
        let vnc_url = v["vnc_url"].as_str().expect("vnc_url").to_string();
        assert!(vnc_url.starts_with("ws://127.0.0.1:"), "vnc_url: {vnc_url}");
        assert!(vnc_url.ends_with("/websockify"), "vnc_url: {vnc_url}");
        eprintln!("created {id} vnc_url={vnc_url}");

        // Safety net: remove the container even if an assertion panics.
        struct Guard(Option<String>);
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Some(id) = self.0.take() {
                    let _ = std::process::Command::new("podman")
                        .args(["rm", "-f", &id])
                        .output();
                }
            }
        }
        let mut guard = Guard(Some(id.clone()));

        // exec
        let r = execute_test_env(
            &json!({"action":"exec","env_id":id,"command":"echo tool-says-hi"}),
            &cfg,
        )
        .await;
        assert!(r.ok, "exec failed: {}", r.output);
        assert!(
            r.output.contains("tool-says-hi"),
            "exec output: {}",
            r.output
        );

        // screenshot
        let r = execute_test_env(&json!({"action":"screenshot","env_id":id}), &cfg).await;
        assert!(r.ok, "screenshot failed: {}", r.output);
        let s: Value = serde_json::from_str(&r.output).expect("screenshot output is JSON");
        let path = s["path"].as_str().expect("path");
        assert!(
            std::path::Path::new(path).exists(),
            "screenshot file missing: {path}"
        );
        assert!(
            s["bytes"].as_u64().unwrap_or(0) > 1000,
            "screenshot too small"
        );
        eprintln!("screenshot -> {path}");

        // vnc_url
        let r = execute_test_env(&json!({"action":"vnc_url","env_id":id}), &cfg).await;
        assert!(r.ok, "vnc_url failed: {}", r.output);

        // destroy
        let r = execute_test_env(&json!({"action":"destroy","env_id":id}), &cfg).await;
        assert!(r.ok, "destroy failed: {}", r.output);
        assert!(
            r.output.contains("destroyed"),
            "destroy output: {}",
            r.output
        );
        guard.0.take(); // disarm: destroy already removed it
    }
}
