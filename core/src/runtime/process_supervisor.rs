//! Project-scoped supervision for long-lived named processes.
//!
//! The caller is responsible for obtaining approval and selecting the execution
//! boundary before constructing a [`ProcessSpec`].  This module deliberately
//! accepts argv, never a shell command string, and owns only process lifecycle
//! and durable project state.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_LOG_BYTES: usize = 64 * 1024;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
static SUPERVISORS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<HashMap<String, ManagedProcess>>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn supervisors() -> &'static Mutex<HashMap<PathBuf, Arc<Mutex<HashMap<String, ManagedProcess>>>>> {
    &SUPERVISORS
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub env: HashMap<String, String>,
    /// A workspace-relative directory.  It is constrained before spawn.
    pub cwd: PathBuf,
    pub readiness: Readiness,
    pub ready_timeout: Duration,
    pub log_capacity: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Readiness {
    pub log_regex: Option<String>,
    pub tcp_port: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub name: String,
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub started_at_ms: u128,
    pub state: ProcessState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Starting,
    Ready,
    Exited,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessLogs {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessError {
    Invalid(String),
    NotFound(String),
    AlreadyRunning(String),
    ReadinessTimeout(String),
    Io(String),
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(s)
            | Self::NotFound(s)
            | Self::AlreadyRunning(s)
            | Self::ReadinessTimeout(s)
            | Self::Io(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for ProcessError {}

#[derive(Serialize, Deserialize)]
struct PersistedProcess {
    status: ProcessStatus,
    log_capacity: usize,
}

struct ManagedProcess {
    child: Child,
    logs: Arc<Mutex<RingLog>>,
}

#[derive(Default)]
struct RingLog {
    bytes: VecDeque<u8>,
    capacity: usize,
    dropped: bool,
}

impl RingLog {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            capacity,
            dropped: false,
        }
    }
    fn push(&mut self, data: &[u8]) {
        for byte in data {
            if self.bytes.len() == self.capacity {
                self.bytes.pop_front();
                self.dropped = true;
            }
            self.bytes.push_back(*byte);
        }
    }
    fn snapshot(&self) -> ProcessLogs {
        let data: Vec<u8> = self.bytes.iter().copied().collect();
        ProcessLogs {
            text: String::from_utf8_lossy(&data).into_owned(),
            truncated: self.dropped,
        }
    }
}

/// A supervisor namespace is one workspace.  Its registry survives core
/// restarts, while child handles only optimize local status and log access.
pub struct NamedProcessSupervisor {
    workspace: PathBuf,
    state_dir: PathBuf,
    managed: Arc<Mutex<HashMap<String, ManagedProcess>>>,
}
impl Drop for NamedProcessSupervisor {
    fn drop(&mut self) {
        let mut registry = supervisors().lock().unwrap_or_else(|e| e.into_inner());
        let Some(current) = registry.get(&self.workspace) else {
            return;
        };
        // During Drop, this instance contributes one Arc strong reference; the
        // registry contributes the other. No additional owners may remain.
        if Arc::strong_count(current) != 2
            || !self
                .managed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            || !Arc::ptr_eq(current, &self.managed)
        {
            return;
        }
        registry.remove(&self.workspace);
    }
}

#[cfg(test)]
fn registry_len() -> usize {
    supervisors()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}
impl NamedProcessSupervisor {
    pub fn new(workspace: impl Into<PathBuf>) -> Result<Self, ProcessError> {
        let workspace = fs::canonicalize(workspace.into()).map_err(io_error)?;
        let state_dir = workspace.join(".catalyst-code").join("processes");
        fs::create_dir_all(&state_dir).map_err(io_error)?;
        let managed = supervisors()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(workspace.clone())
            .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())))
            .clone();
        Ok(Self {
            workspace,
            state_dir,
            managed,
        })
    }

    /// Starts argv directly after acquiring this project's durable name lock.
    /// A readiness condition is optional; without one a successful spawn is ready.
    pub fn start(&self, spec: ProcessSpec) -> Result<ProcessStatus, ProcessError> {
        validate_spec(&spec)?;
        self.recover_stale(&spec.name)?;
        let mut lock = self.acquire_lock(&spec.name)?;
        if self.record_path(&spec.name).exists() {
            return Err(ProcessError::AlreadyRunning(format!(
                "process {:?} is already registered",
                spec.name
            )));
        }
        let cwd = self.resolve_cwd(&spec.cwd)?;
        let log_capacity = spec.log_capacity.clamp(1, MAX_LOG_BYTES);
        let mut command = Command::new(&spec.argv[0]);
        command
            .args(&spec.argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(io_error)?;
        let pid = child.id();
        let started_at_ms = now_ms();
        let logs = Arc::new(Mutex::new(RingLog::with_capacity(log_capacity)));
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        spawn_log_drain(stdout, logs.clone());
        spawn_log_drain(stderr, logs.clone());
        let mut status = ProcessStatus {
            name: spec.name.clone(),
            pid,
            argv: spec.argv,
            cwd,
            started_at_ms,
            state: ProcessState::Starting,
        };
        self.write_record(&status, log_capacity)?;
        lock.retain = true;
        self.managed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                spec.name.clone(),
                ManagedProcess {
                    child,
                    logs: logs.clone(),
                },
            );
        match self.wait_ready(&status.name, &spec.readiness, spec.ready_timeout) {
            Ok(()) => {
                status.state = ProcessState::Ready;
                self.write_record(&status, log_capacity)?;
                Ok(status)
            }
            Err(error) => {
                let _ = self.stop(&status.name);
                Err(error)
            }
        }
    }

    pub fn status(&self, name: &str) -> Result<ProcessStatus, ProcessError> {
        self.recover_stale(name)?;
        let persisted = self.read_record(name)?;
        let mut status = persisted.status;
        if !process_alive(status.pid) {
            status.state = ProcessState::Exited;
            self.write_record(&status, persisted.log_capacity)?;
        }
        Ok(status)
    }

    pub fn logs(&self, name: &str) -> Result<ProcessLogs, ProcessError> {
        self.recover_stale(name)?;
        if let Some(process) = self
            .managed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            return Ok(process
                .logs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .snapshot());
        }
        Err(ProcessError::NotFound(format!(
            "process {name:?} is not owned by this core instance; live logs are unavailable"
        )))
    }

    pub fn stop(&self, name: &str) -> Result<ProcessStatus, ProcessError> {
        let persisted = self.read_record(name)?;
        terminate_process_group(persisted.status.pid)?;
        if let Some(mut process) = self
            .managed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
        {
            let _ = process.child.wait();
        }
        let mut status = persisted.status;
        status.state = ProcessState::Exited;
        self.write_record(&status, persisted.log_capacity)?;
        self.remove_lock(name);
        Ok(status)
    }

    pub fn restart(&self, spec: ProcessSpec) -> Result<ProcessStatus, ProcessError> {
        if self.record_path(&spec.name).exists() {
            self.stop(&spec.name)?;
        }
        self.start(spec)
    }

    fn wait_ready(
        &self,
        name: &str,
        readiness: &Readiness,
        timeout: Duration,
    ) -> Result<(), ProcessError> {
        if readiness.log_regex.is_none() && readiness.tcp_port.is_none() {
            return Ok(());
        }
        let regex = readiness
            .log_regex
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|e| ProcessError::Invalid(format!("invalid readiness regex: {e}")))?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !process_alive(self.read_record(name)?.status.pid) {
                return Err(ProcessError::ReadinessTimeout(format!(
                    "process {name:?} exited before readiness"
                )));
            }
            let log_ready = regex
                .as_ref()
                .is_none_or(|re| self.logs(name).is_ok_and(|logs| re.is_match(&logs.text)));
            let port_ready = readiness.tcp_port.is_none_or(|port| {
                TcpStream::connect_timeout(
                    &format!("127.0.0.1:{port}").parse().expect("valid socket"),
                    Duration::from_millis(50),
                )
                .is_ok()
            });
            if log_ready && port_ready {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(ProcessError::ReadinessTimeout(format!(
                    "process {name:?} did not become ready within {} ms",
                    timeout.as_millis()
                )));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn resolve_cwd(&self, input: &Path) -> Result<PathBuf, ProcessError> {
        let resolved = crate::workspace::resolve(
            &self.workspace,
            input
                .to_str()
                .ok_or_else(|| ProcessError::Invalid("cwd must be valid UTF-8".into()))?,
        )
        .map_err(ProcessError::Invalid)?;
        if !resolved.is_dir() {
            return Err(ProcessError::Invalid(format!(
                "cwd {:?} is not a directory",
                input
            )));
        }
        Ok(resolved)
    }
    fn record_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.json"))
    }
    fn lock_path(&self, name: &str) -> PathBuf {
        self.state_dir.join(format!("{name}.lock"))
    }
    fn read_record(&self, name: &str) -> Result<PersistedProcess, ProcessError> {
        let data = fs::read(self.record_path(name))
            .map_err(|_| ProcessError::NotFound(format!("no process named {name:?}")))?;
        serde_json::from_slice(&data)
            .map_err(|e| ProcessError::Io(format!("invalid process registry: {e}")))
    }
    fn write_record(
        &self,
        status: &ProcessStatus,
        log_capacity: usize,
    ) -> Result<(), ProcessError> {
        atomic_write(
            &self.record_path(&status.name),
            &serde_json::to_vec(&PersistedProcess {
                status: status.clone(),
                log_capacity,
            })
            .expect("serializable"),
        )
    }
    fn acquire_lock(&self, name: &str) -> Result<LockGuard, ProcessError> {
        use std::io::Write;
        let path = self.lock_path(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| ProcessError::AlreadyRunning(format!("process {name:?} is locked")))?;
        write!(file, "{}", std::process::id()).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        Ok(LockGuard {
            path,
            retain: false,
        })
    }
    fn remove_lock(&self, name: &str) {
        let _ = fs::remove_file(self.lock_path(name));
    }
    fn recover_stale(&self, name: &str) -> Result<(), ProcessError> {
        if let Ok(record) = self.read_record(name) {
            if !process_alive(record.status.pid) {
                let _ = fs::remove_file(self.record_path(name));
                self.remove_lock(name);
            }
            return Ok(());
        }
        let lock = self.lock_path(name);
        if let Ok(owner) = fs::read_to_string(&lock) {
            if owner
                .trim()
                .parse::<u32>()
                .ok()
                .is_none_or(|pid| !process_alive(pid))
            {
                let _ = fs::remove_file(lock);
            }
        }
        Ok(())
    }
}

struct LockGuard {
    path: PathBuf,
    retain: bool,
}
impl Drop for LockGuard {
    fn drop(&mut self) {
        if !self.retain {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_spec(spec: &ProcessSpec) -> Result<(), ProcessError> {
    if spec.name.is_empty()
        || !spec
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ProcessError::Invalid(
            "process name must contain only ASCII letters, digits, '-' or '_'".into(),
        ));
    }
    if spec.argv.is_empty() || spec.argv[0].is_empty() {
        return Err(ProcessError::Invalid("argv must contain a program".into()));
    }
    if spec.log_capacity > MAX_LOG_BYTES {
        return Err(ProcessError::Invalid(format!(
            "log capacity exceeds {MAX_LOG_BYTES} bytes"
        )));
    }
    Ok(())
}
fn spawn_log_drain<R: Read + Send + 'static>(mut reader: R, logs: Arc<Mutex<RingLog>>) {
    std::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(count) => logs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(&buf[..count]),
            }
        }
    });
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn io_error(error: std::io::Error) -> ProcessError {
    ProcessError::Io(error.to_string())
}
fn atomic_write(path: &Path, data: &[u8]) -> Result<(), ProcessError> {
    let temp = path.with_extension(format!("{}.tmp", now_ms()));
    fs::write(&temp, data).map_err(io_error)?;
    fs::rename(temp, path).map_err(io_error)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}
#[cfg(not(unix))]
fn configure_process_group(_: &mut Command) {}
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    unsafe {
        libc::kill(pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
#[cfg(not(unix))]
fn process_alive(_: u32) -> bool {
    true
}
#[cfg(unix)]
fn terminate_process_group(pid: u32) -> Result<(), ProcessError> {
    unsafe {
        if libc::kill(-(pid as i32), libc::SIGTERM) != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while process_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(POLL_INTERVAL);
    }
    if process_alive(pid) {
        unsafe {
            if libc::kill(-(pid as i32), libc::SIGKILL) != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn terminate_process_group(_: u32) -> Result<(), ProcessError> {
    Err(ProcessError::Io(
        "named process groups are unsupported on this platform".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn workspace() -> PathBuf {
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("catalyst-process-supervisor-{id}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[cfg(unix)]
    #[test]
    fn start_ready_logs_stop_and_log_cap() {
        let ws = workspace();
        let supervisor = NamedProcessSupervisor::new(&ws).unwrap();
        let state = supervisor.start(spec("web", &["sh", "-c", "printf 1234567890123456789012345678901234567890123456789012345678901234567890; printf ready; sleep 30"])).unwrap();
        assert_eq!(state.state, ProcessState::Ready);
        std::thread::sleep(Duration::from_millis(50));
        let logs = supervisor.logs("web").unwrap();
        assert!(logs.text.contains("ready"));
        assert!(logs.truncated);
        assert_eq!(supervisor.stop("web").unwrap().state, ProcessState::Exited);
        let _ = fs::remove_dir_all(ws);
    }
    fn spec(name: &str, args: &[&str]) -> ProcessSpec {
        ProcessSpec {
            name: name.into(),
            argv: args.iter().map(|s| (*s).into()).collect(),
            env: HashMap::new(),
            cwd: PathBuf::from("."),
            readiness: Readiness {
                log_regex: Some("ready".into()),
                tcp_port: None,
            },
            ready_timeout: Duration::from_secs(2),
            log_capacity: 64,
        }
    }
    #[cfg(unix)]
    #[test]
    fn name_lock_and_project_isolation() {
        let one = workspace();
        let two = workspace();
        let a = NamedProcessSupervisor::new(&one).unwrap();
        let b = NamedProcessSupervisor::new(&one).unwrap();
        let c = NamedProcessSupervisor::new(&two).unwrap();
        a.start(spec("same", &["sh", "-c", "echo ready; sleep 30"]))
            .unwrap();
        assert!(matches!(
            b.start(spec("same", &["sh", "-c", "echo ready; sleep 30"])),
            Err(ProcessError::AlreadyRunning(_))
        ));
        c.start(spec("same", &["sh", "-c", "echo ready; sleep 30"]))
            .unwrap();
        a.stop("same").unwrap();
        c.stop("same").unwrap();
        let _ = fs::remove_dir_all(one);
        let _ = fs::remove_dir_all(two);
    }
    #[test]
    fn registry_shares_live_instances_and_prunes_after_drop() {
        let ws = workspace();
        let before = registry_len();
        let first = NamedProcessSupervisor::new(&ws).unwrap();
        let second = NamedProcessSupervisor::new(&ws).unwrap();
        assert_eq!(registry_len(), before + 1);
        assert!(Arc::ptr_eq(&first.managed, &second.managed));
        drop(first);
        assert_eq!(registry_len(), before + 1);
        drop(second);
        assert_eq!(registry_len(), before);
        let _ = fs::remove_dir_all(ws);
    }
    #[cfg(unix)]
    #[test]
    fn readiness_timeout_and_stale_recovery() {
        let ws = workspace();
        let supervisor = NamedProcessSupervisor::new(&ws).unwrap();
        let mut never = spec("slow", &["sh", "-c", "sleep 30"]);
        never.ready_timeout = Duration::from_millis(80);
        assert!(matches!(
            supervisor.start(never),
            Err(ProcessError::ReadinessTimeout(_))
        ));
        let stale = PersistedProcess {
            status: ProcessStatus {
                name: "stale".into(),
                pid: u32::MAX,
                argv: vec!["none".into()],
                cwd: ws.clone(),
                started_at_ms: 0,
                state: ProcessState::Ready,
            },
            log_capacity: DEFAULT_LOG_BYTES,
        };
        fs::write(
            supervisor.record_path("stale"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        fs::write(supervisor.lock_path("stale"), u32::MAX.to_string()).unwrap();
        supervisor
            .start(spec("stale", &["sh", "-c", "echo ready; sleep 30"]))
            .unwrap();
        supervisor.stop("stale").unwrap();
        let _ = fs::remove_dir_all(ws);
    }
}
