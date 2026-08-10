// Session persistence: append-only JSONL of conversation messages, prefixed
// by a schema-version header line so future shape changes can migrate old
// files instead of silently misreading them. On init, if the session file
// exists it's loaded and replayed; each finalized message is appended (and
// fsync'd) so a crash mid-task loses at most the in-flight turn.
use crate::message::Message;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Bump when the on-disk message shape changes. load() validates the header.
pub const SESSION_VERSION: u32 = 2;

fn header_line() -> String {
    format!("{{\"_session_version\": {}}}", SESSION_VERSION)
}

fn ensure_header(path: &Path) {
    // Create the file with a header if it doesn't exist yet.
    if path.exists() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", header_line());
        let _ = f.flush();
        let _ = f.sync_all();
    }
}

/// Create the session file with just its version header if it doesn't already
/// exist. Used so a new/active session shows up in `list_sessions`
/// immediately, even before the first message is appended.
pub fn ensure(path: &Path) {
    ensure_header(path);
    migrate_header(path);
}

fn migrate_header(path: &Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Some((first, rest)) = content.split_once('\n') else {
        return;
    };
    let Ok(header) = serde_json::from_str::<Value>(first) else {
        return;
    };
    let Some(version) = header.get("_session_version").and_then(Value::as_u64) else {
        return;
    };
    if version as u32 >= SESSION_VERSION {
        return;
    }
    let migrated = format!("{}\n{}", header_line(), rest);
    let _ = crate::fsutil::atomic_write_str(path, &migrated);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Started,
    Completed,
    Cancelled,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RunRecord {
    pub session_id: String,
    pub run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub state: RunState,
    pub timestamp_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ParentDelivery {
    pub parent_run_id: String,
    pub run_id: String,
    pub artifact_path: PathBuf,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CompactionRecord {
    pub id: String,
    #[serde(default, rename = "parentId")]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AbandonedBranch {
    pub from: String,
    pub to: String,
    pub summary: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default, rename = "parentId")]
    pub parent_id: Option<String>,
    /// Branch markers carry no transcript message. The next real append becomes
    /// their child, so branching never injects synthetic content into context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
}

fn next_compaction_id() -> String {
    use rand::Rng;
    format!("compaction-{:016x}", rand::thread_rng().gen::<u64>())
}

fn next_entry_id() -> String {
    use rand::Rng;
    format!("entry-{:016x}", rand::thread_rng().gen::<u64>())
}

fn leaf_path(path: &Path) -> PathBuf {
    path.with_extension("leaf.json")
}

fn read_active_leaf(path: &Path) -> Result<Option<String>, String> {
    let sidecar = leaf_path(path);
    let raw = match std::fs::read_to_string(&sidecar) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read active leaf {}: {error}", sidecar.display())),
    };
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("malformed active leaf {}: {error}", sidecar.display()))?;
    let leaf = value
        .get("leaf")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "malformed active leaf {}: missing leaf id",
                sidecar.display()
            )
        })?;
    Ok(Some(leaf.to_string()))
}

pub fn active_leaf(path: &Path) -> Option<String> {
    read_active_leaf(path).ok().flatten()
}

fn set_active_leaf(path: &Path, id: &str) -> Result<(), String> {
    crate::fsutil::atomic_write_str(
        &leaf_path(path),
        &serde_json::json!({"leaf":id}).to_string(),
    )
    .map_err(|e| format!("write active leaf: {e}"))
}

#[derive(Debug, Default)]
pub struct LoadReport {
    pub messages: Vec<Message>,
    pub warnings: Vec<String>,
    pub unfinished_runs: Vec<RunRecord>,
    pub parent_deliveries: Vec<ParentDelivery>,
    pub entries: Vec<SessionEntry>,
    pub compactions: Vec<CompactionRecord>,
    pub abandoned_branches: Vec<AbandonedBranch>,
}

fn tree_metadata(
    entries: &[SessionEntry],
    leaf: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let mut by_id = std::collections::HashMap::new();
    for entry in entries {
        if entry.id.trim().is_empty() {
            return Err("session tree contains an empty entry id".into());
        }
        if by_id.insert(entry.id.as_str(), entry).is_some() {
            return Err(format!(
                "session tree contains duplicate entry id {}",
                entry.id
            ));
        }
    }
    for entry in entries {
        if let Some(parent) = entry.parent_id.as_deref() {
            if parent == entry.id {
                return Err(format!("session entry {} points to itself", entry.id));
            }
            if !by_id.contains_key(parent) {
                return Err(format!(
                    "session entry {} points to missing parent {}",
                    entry.id, parent
                ));
            }
        }
    }
    let Some(leaf) = leaf else {
        return Ok((Vec::new(), Vec::new()));
    };
    if !by_id.contains_key(leaf) {
        return Err(format!("active session leaf {leaf} is unknown"));
    }
    let mut ancestry = Vec::new();
    let mut cursor = Some(leaf);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id) {
            return Err("session tree contains a parent cycle".into());
        }
        ancestry.push(id.to_string());
        cursor = by_id.get(id).and_then(|entry| entry.parent_id.as_deref());
    }
    ancestry.reverse();
    let parent = by_id.get(leaf).and_then(|entry| entry.parent_id.as_deref());
    let siblings = entries
        .iter()
        .filter(|entry| entry.parent_id.as_deref() == parent && entry.id != leaf)
        .map(|entry| entry.id.clone())
        .collect();
    Ok((ancestry, siblings))
}

pub fn session_tree(path: &Path) -> Value {
    let report = match load_report(path) {
        Ok(report) => report,
        Err(error) => {
            return serde_json::json!({"leaf": Value::Null, "entries": [], "compactions": [], "abandonedBranches": [], "ancestry": [], "siblings": [], "warning": error})
        }
    };
    let leaf = match read_active_leaf(path) {
        Ok(leaf) => leaf.or_else(|| report.entries.last().map(|entry| entry.id.clone())),
        Err(error) => {
            return serde_json::json!({"leaf": Value::Null, "entries": report.entries, "compactions": report.compactions, "abandonedBranches": report.abandoned_branches, "ancestry": [], "siblings": [], "warning": error})
        }
    };
    match tree_metadata(&report.entries, leaf.as_deref()) {
        Ok((ancestry, siblings)) => serde_json::json!({
            "leaf": leaf,
            "entries": report.entries,
            "compactions": report.compactions,
            "abandonedBranches": report.abandoned_branches,
            "ancestry": ancestry,
            "siblings": siblings,
        }),
        Err(error) => {
            serde_json::json!({"leaf": Value::Null, "entries": report.entries, "compactions": report.compactions, "abandonedBranches": report.abandoned_branches, "ancestry": [], "siblings": [], "warning": error})
        }
    }
}

pub fn create_branch(path: &Path, entry_id: &str) -> Result<String, String> {
    let report = load_report(path)?;
    tree_metadata(&report.entries, Some(entry_id))?;
    if let Some(from) = active_leaf(path).filter(|from| from != entry_id) {
        let by_id: std::collections::HashMap<&str, &SessionEntry> = report
            .entries
            .iter()
            .map(|entry| (entry.id.as_str(), entry))
            .collect();
        let mut cursor = Some(from.as_str());
        let mut abandoned = Vec::new();
        while let Some(id) = cursor {
            if id == entry_id {
                break;
            }
            let Some(entry) = by_id.get(id) else { break };
            if let Some(text) = entry.message.as_ref().and_then(Message::content_text) {
                abandoned.push(text.to_string());
            }
            cursor = entry.parent_id.as_deref();
        }
        if !abandoned.is_empty() {
            abandoned.reverse();
            let mut summary = abandoned.join(" | ");
            const MAX_BRANCH_SUMMARY_CHARS: usize = 512;
            if summary.chars().count() > MAX_BRANCH_SUMMARY_CHARS {
                summary = summary.chars().take(MAX_BRANCH_SUMMARY_CHARS - 1).collect();
                summary.push('…');
            }
            append_branch_summary(path, &from, entry_id, &summary)?;
        }
    }
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
        .map_err(|e| format!("lock session branch: {e}"))?;
    ensure_record_boundary(path);
    let id = next_entry_id();
    let entry = SessionEntry {
        id: id.clone(),
        parent_id: Some(entry_id.to_string()),
        message: None,
    };
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("open session branch: {e}"))?;
    writeln!(file, "{}", serde_json::json!({"_entry": entry}))
        .map_err(|e| format!("append session branch: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync session branch: {e}"))?;
    set_active_leaf(path, &id)?;
    Ok(id)
}

pub fn append_run_state(
    path: &Path,
    session_id: &str,
    run_id: &str,
    state: RunState,
    detail: Option<&str>,
) {
    append_activity_state(path, session_id, run_id, "run", None, None, state, detail);
}

/// Append a lifecycle record for foreground or child activity. New optional
/// identity fields are backward-compatible with v2 journals and let recovery
/// distinguish an interrupted tool, subagent, or goal without ever restarting it.
#[allow(clippy::too_many_arguments)]
pub fn append_activity_state(
    path: &Path,
    session_id: &str,
    run_id: &str,
    kind: &str,
    parent_run_id: Option<&str>,
    tool_call_id: Option<&str>,
    state: RunState,
    detail: Option<&str>,
) {
    ensure_header(path);
    // Same lock as append/rewrite so run-state lines cannot interleave
    // with message entries (CORE_REVIEW).
    let Ok(_lock) = crate::fsutil::FileLock::acquire(&path.with_extension("lock")) else {
        eprintln!(
            "[session] activity_state lock failed for {}",
            path.display()
        );
        return;
    };
    ensure_record_boundary(path);
    let record = RunRecord {
        session_id: session_id.to_string(),
        run_id: run_id.to_string(),
        kind: Some(kind.to_string()),
        parent_run_id: parent_run_id.map(str::to_string),
        tool_call_id: tool_call_id.map(str::to_string),
        state,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        detail: detail.map(str::to_string),
    };
    let Ok(mut file) = OpenOptions::new().append(true).open(path) else {
        return;
    };
    let line = serde_json::json!({"_run": record});
    let _ = writeln!(file, "{line}");
    let _ = file.flush();
}

pub fn append(path: &Path, msg: &Message) {
    ensure_header(path);
    let Ok(_lock) = crate::fsutil::FileLock::acquire(&path.with_extension("lock")) else {
        eprintln!(
            "[session] append lock failed for {}; message not persisted",
            path.display()
        );
        return;
    };
    ensure_record_boundary(path);
    let id = next_entry_id();
    let entry = SessionEntry {
        id: id.clone(),
        parent_id: active_leaf(path),
        message: Some(msg.clone()),
    };
    let Ok(mut file) = OpenOptions::new().append(true).open(path) else {
        eprintln!(
            "[session] append open failed for {}; message not persisted",
            path.display()
        );
        return;
    };
    let line = serde_json::json!({"_entry": entry});
    if writeln!(file, "{line}").is_err() || file.flush().is_err() {
        eprintln!(
            "[session] append write failed for {}; message not persisted",
            path.display()
        );
        return;
    }
    let _ = file.sync_all();
    let _ = set_active_leaf(path, &id);
}

fn ensure_record_boundary(path: &Path) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = OpenOptions::new().read(true).append(true).open(path) else {
        return;
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return;
    };
    if length == 0 || file.seek(SeekFrom::End(-1)).is_err() {
        return;
    }
    let mut last = [0_u8; 1];
    if file.read_exact(&mut last).is_ok() && last[0] != b'\n' {
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn artifact_path(session_path: &Path, run_id: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(run_id.as_bytes());
    let hash = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    session_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("artifacts")
        .join(format!("job-{hash}.json"))
}

pub fn write_job_artifact(
    session_path: &Path,
    run_id: &str,
    parent_run_id: Option<&str>,
    state: &str,
    summary: &str,
) -> Result<PathBuf, String> {
    let now = now_ms();
    write_job_artifact_record(
        session_path,
        run_id,
        parent_run_id,
        run_id,
        state,
        now,
        now,
        (state == "failed").then_some(summary),
        summary,
    )
}

/// Persist the complete durable job record. The artifact is session-owned and
/// its own path is recorded as the result reference so restart can inspect it.
pub fn write_job_artifact_record(
    session_path: &Path,
    run_id: &str,
    parent_run_id: Option<&str>,
    task_identity: &str,
    state: &str,
    started_at: u64,
    ended_at: u64,
    error: Option<&str>,
    summary: &str,
) -> Result<PathBuf, String> {
    let path = artifact_path(session_path, run_id);
    std::fs::create_dir_all(path.parent().unwrap())
        .map_err(|e| format!("create artifact dir: {e}"))?;
    let body = serde_json::json!({
        "run_id": run_id,
        "parent_run_id": parent_run_id,
        "task_identity": task_identity,
        "state": state,
        "started_at": started_at,
        "ended_at": ended_at,
        "error": error,
        "summary": summary,
        "result_ref": path.display().to_string(),
    });
    crate::fsutil::atomic_write_str(&path, &(body.to_string() + "\n"))
        .map_err(|e| format!("write job artifact: {e}"))?;
    Ok(path)
}

pub fn read_job_artifact(session_path: &Path, run_id: &str) -> Option<Value> {
    std::fs::read_to_string(artifact_path(session_path, run_id))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(|value| value.get("run_id").and_then(Value::as_str) == Some(run_id))
}

fn artifact_reference_run_id(reference: &str) -> Option<&str> {
    let value = reference.strip_prefix("artifact://")?;
    let filename = value.rsplit('/').next().unwrap_or(value);
    filename.strip_suffix(".json").or(Some(filename))
}

pub fn artifact_run_ids_referenced_by_messages(
    messages: &[Message],
) -> std::collections::HashSet<String> {
    let mut retained = std::collections::HashSet::new();
    for message in messages {
        let Some(content) = message.content_text() else {
            continue;
        };
        for value in content
            .match_indices("artifact://")
            .map(|(offset, _)| &content[offset..])
        {
            let reference = value.split_whitespace().next().unwrap_or(value);
            if let Some(id) = artifact_reference_run_id(reference) {
                retained.insert(id.to_string());
            }
        }
    }
    retained
}

pub fn shake_artifacts(
    session_path: &Path,
    keep_run_ids: &std::collections::HashSet<String>,
) -> Result<usize, String> {
    let Some(dir) = session_path.parent().map(|parent| parent.join("artifacts")) else {
        return Ok(0);
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut retained = keep_run_ids.clone();
    if let Ok(report) = load_report(session_path) {
        retained.extend(
            report
                .parent_deliveries
                .into_iter()
                .map(|delivery| delivery.run_id),
        );
        for compaction in report.compactions {
            for reference in compaction.artifacts {
                if let Some(id) = artifact_reference_run_id(&reference) {
                    retained.insert(id.to_string());
                }
            }
        }
    }
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let keep = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("run_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|id| retained.contains(&id));
        if !keep && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn append_parent_delivery(
    session_path: &Path,
    parent_run_id: &str,
    run_id: &str,
    artifact: &Path,
) -> Result<(), String> {
    ensure_header(session_path);
    let _lock = crate::fsutil::FileLock::acquire(&session_path.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    let existing = std::fs::read_to_string(session_path).unwrap_or_default();
    if existing
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|value| {
            let Some(delivery) = value.get("_parent_delivery") else {
                return false;
            };
            delivery.get("run_id").and_then(Value::as_str) == Some(run_id)
                && delivery.get("parent_run_id").and_then(Value::as_str) == Some(parent_run_id)
        })
    {
        return Ok(());
    }
    ensure_record_boundary(session_path);
    let mut file = OpenOptions::new()
        .append(true)
        .open(session_path)
        .map_err(|e| e.to_string())?;
    writeln!(file, "{}", serde_json::json!({"_parent_delivery":{"parent_run_id":parent_run_id,"run_id":run_id,"artifact_path":artifact}})).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())
}

pub fn append_compaction(
    path: &Path,
    messages: &[Message],
    summary: &str,
    artifacts: &[String],
) -> Result<(), String> {
    ensure_header(path);
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
        .map_err(|e| format!("lock compaction journal: {e}"))?;
    ensure_record_boundary(path);
    let parent_id = active_leaf(path);
    let record = CompactionRecord {
        id: next_compaction_id(),
        parent_id: parent_id.clone(),
        root: parent_id,
        summary: summary.to_string(),
        artifacts: artifacts.to_vec(),
    };
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("open compaction journal: {e}"))?;
    let line = serde_json::json!({"_compaction": {
        "id": record.id,
        "parentId": record.parent_id,
        "root": record.root,
        "summary": record.summary,
        "artifacts": record.artifacts,
        "messages": messages,
    }});
    writeln!(file, "{line}").map_err(|e| format!("append compaction journal: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync compaction journal: {e}"))
}

fn append_branch_summary(path: &Path, from: &str, to: &str, summary: &str) -> Result<(), String> {
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
        .map_err(|e| format!("lock branch journal: {e}"))?;
    ensure_record_boundary(path);
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("open branch journal: {e}"))?;
    writeln!(
        file,
        "{}",
        serde_json::json!({"_abandoned_branch": {"from": from, "to": to, "summary": summary}})
    )
    .map_err(|e| format!("append branch journal: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync branch journal: {e}"))
}
/// fsync the session file so finalized turns survive a crash. Call at turn
/// end (and on abort paths that have already appended results).
pub fn sync(path: &Path) {
    if let Ok(f) = OpenOptions::new().append(true).open(path) {
        let _ = f.sync_all();
    }
    if let Some(parent) = path.parent() {
        fsync_dir(parent);
    }
}

/// Load all messages from a session file. Skips the version header and any
/// unparseable lines. Returns `Ok(Vec)` for a missing file (nothing to resume)
/// or a current-version file. Returns `Err(human_message)` when the file's
/// header version is NEWER than `SESSION_VERSION` — refusing to silently
/// misread/drop a session on upgrade (the caller surfaces the error to the
/// user instead of quietly starting blank).
pub fn load(path: &Path) -> Result<Vec<Message>, String> {
    load_report(path).map(|report| report.messages)
}

/// Load readable conversation records and return explicit recovery details.
/// Malformed records do not erase valid history; an incomplete final line is
/// treated as a crash-truncated append, while malformed interior lines are
/// counted separately. Started runs without a terminal record are returned so
/// startup can persist an `interrupted` terminal state without rerunning work.
pub fn load_report(path: &Path) -> Result<LoadReport, String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Ok(LoadReport::default());
    };
    let nonempty: Vec<(usize, &str)> = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .collect();
    let mut lines = nonempty.iter();
    // First non-empty line must be the version header. If it's absent or a
    // future version, bail with a clear error rather than guess.
    let first = lines.next().map(|(_, line)| *line).unwrap_or("");
    let mut report = LoadReport::default();
    let mut run_states = std::collections::HashMap::<String, RunRecord>::new();
    let mut first_is_message = false;
    if let Ok(v) = serde_json::from_str::<Value>(first) {
        if let Some(ver) = v.get("_session_version").and_then(|x| x.as_u64()) {
            if ver as u32 > SESSION_VERSION {
                return Err(format!(
                    "session file {} is version {ver}, newer than supported ({SESSION_VERSION}); not loaded to avoid corrupting it. Delete the file (or migrate it) to continue.",
                    path.display()
                ));
            }
            if (ver as u32) < SESSION_VERSION {
                report.warnings.push(format!(
                    "session schema v{ver} loaded through compatibility mode; it will migrate to v{SESSION_VERSION} before the next append"
                ));
            }
        } else {
            first_is_message = true;
            report.warnings.push(
                "legacy session without a version header loaded in compatibility mode".into(),
            );
        }
    } else if !first.is_empty() {
        first_is_message = true;
    }

    let records: Vec<(usize, &str)> = if first_is_message {
        nonempty.clone()
    } else {
        nonempty.into_iter().skip(1).collect()
    };
    let last_line = records.last().map(|(line, _)| *line);
    let final_line_terminated = content.ends_with('\n');
    let mut compaction_root: Option<String> = None;
    let mut malformed = Vec::new();
    for (line_number, line) in records {
        let line_number = line_number + 1;
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                if Some(line_number - 1) == last_line && !final_line_terminated {
                    report.warnings.push(format!(
                        "recovered session after ignoring a truncated final record at line {line_number}"
                    ));
                } else {
                    malformed.push(line_number);
                }
                continue;
            }
        };
        if let Some(run) = value.get("_run") {
            match serde_json::from_value::<RunRecord>(run.clone()) {
                Ok(record) => {
                    run_states.insert(record.run_id.clone(), record);
                }
                Err(_) => malformed.push(line_number),
            }
            continue;
        }
        if let Some(compaction) = value.get("_compaction") {
            if let Ok(record) = serde_json::from_value::<CompactionRecord>(compaction.clone()) {
                report.compactions.push(record);
            }
            report.messages.clear();
            compaction_root = compaction
                .get("root")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(messages) = compaction.get("messages").and_then(Value::as_array) {
                for message in messages {
                    if let Ok(mut parsed) = serde_json::from_value::<Message>(message.clone()) {
                        parsed.normalize_embedded_thinking();
                        report.messages.push(parsed);
                    }
                }
            }
            continue;
        }
        if let Some(entry) = value.get("_entry") {
            match serde_json::from_value::<SessionEntry>(entry.clone()) {
                Ok(mut entry) => {
                    if let Some(message) = &mut entry.message {
                        message.normalize_embedded_thinking();
                    }
                    report.entries.push(entry);
                }
                Err(_) => malformed.push(line_number),
            }
            continue;
        }
        if let Some(delivery) = value.get("_parent_delivery") {
            match serde_json::from_value::<ParentDelivery>(delivery.clone()) {
                Ok(record) => report.parent_deliveries.push(record),
                Err(_) => malformed.push(line_number),
            }
            continue;
        }
        match serde_json::from_value::<Message>(value) {
            Ok(mut message) => {
                message.normalize_embedded_thinking();
                report.messages.push(message);
            }
            Err(_) => malformed.push(line_number),
        }
    }
    if !malformed.is_empty() {
        report.warnings.push(format!(
            "ignored {} malformed session record(s) at line(s) {}",
            malformed.len(),
            malformed
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !report.entries.is_empty() {
        let selected_leaf = match read_active_leaf(path) {
            Ok(leaf) => leaf,
            Err(error) => {
                report.warnings.push(error);
                None
            }
        };
        if let Err(error) = tree_metadata(&report.entries, selected_leaf.as_deref()) {
            report.warnings.push(error);
        }
        let by_id: std::collections::HashMap<String, SessionEntry> = report
            .entries
            .iter()
            .cloned()
            .map(|entry| (entry.id.clone(), entry))
            .collect();
        let mut cursor =
            selected_leaf.or_else(|| report.entries.last().map(|entry| entry.id.clone()));
        let mut ancestry = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = cursor {
            if compaction_root.as_deref() == Some(id.as_str()) || !seen.insert(id.clone()) {
                break;
            }
            let Some(entry) = by_id.get(&id) else {
                break;
            };
            if let Some(message) = &entry.message {
                ancestry.push(message.clone());
            }
            cursor = entry.parent_id.clone();
        }
        ancestry.reverse();
        report.messages.extend(ancestry);
    }
    report.unfinished_runs = run_states
        .into_values()
        .filter(|record| record.state == RunState::Started)
        .collect();
    Ok(report)
}

/// Sidecar path for per-session "always" approval escalations (tool kinds the
/// user said "always" to). Stored beside the session file so it travels with
/// the project and survives restart — previously these were in-memory only,
/// so a restart silently un-gated kinds the user had approved.
fn escalations_path(session_path: &Path) -> PathBuf {
    let mut p = session_path.as_os_str().to_os_string();
    p.push(".escalations");
    PathBuf::from(p)
}

/// Load persisted escalated approval kinds (empty set if absent/unreadable).
pub fn load_escalations(session_path: &Path) -> std::collections::HashSet<String> {
    let p = escalations_path(session_path);
    let Ok(content) = std::fs::read_to_string(&p) else {
        return std::collections::HashSet::new();
    };
    serde_json::from_str::<Vec<String>>(&content)
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Best-effort directory fsync after an atomic rename. POSIX does not guarantee
/// a rename survives a power-loss crash unless the parent directory is also
/// fsync'd, so after each temp→target rename we fsync the parent dir. Ignored on
/// platforms where a directory cannot be opened as a file (Windows) — `File::open`
/// on a directory simply fails there and the `if let Ok` skips it.
fn fsync_dir(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// Persist the current set of escalated approval kinds atomically (temp +
/// fsync + rename) so a crash never truncates it.
pub fn save_escalations(session_path: &Path, kinds: &std::collections::HashSet<String>) {
    let p = escalations_path(session_path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = crate::fsutil::unique_tmp(&p);
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
    else {
        return;
    };
    let list: Vec<&String> = kinds.iter().collect();
    let _ = writeln!(f, "{}", serde_json::to_string(&list).unwrap_or_default());
    let _ = f.flush();
    let _ = f.sync_all();
    drop(f); // release before rename (Windows)
    let _ = std::fs::rename(&tmp, &p);
    if let Some(parent) = p.parent() {
        fsync_dir(parent);
    }
}

/// Cumulative session stats persisted beside the session file (sidecar
/// `<session>.stats`) so `/stats` survives a restart — previously these were
/// in-memory only, so reopening the harness showed zeros for tokens/turns.
#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct SessionStats {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cached_tokens: u64,
    pub turns: u64,
    #[serde(default)]
    pub compactions: u64,
}

fn stats_path(session_path: &Path) -> PathBuf {
    let mut p = session_path.as_os_str().to_os_string();
    p.push(".stats");
    PathBuf::from(p)
}

/// Load persisted cumulative stats (all-zero if absent/unreadable).
pub fn load_stats(session_path: &Path) -> SessionStats {
    let p = stats_path(session_path);
    let Ok(content) = std::fs::read_to_string(&p) else {
        return SessionStats::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Persist cumulative stats atomically (temp + fsync + rename) so a crash never
/// truncates them — same durability story as `save_escalations`.
pub fn save_stats(session_path: &Path, stats: &SessionStats) {
    let p = stats_path(session_path);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = crate::fsutil::unique_tmp(&p);
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
    else {
        return;
    };
    let _ = writeln!(f, "{}", serde_json::to_string(stats).unwrap_or_default());
    let _ = f.flush();
    let _ = f.sync_all();
    drop(f); // release before rename (Windows)
    let _ = std::fs::rename(&tmp, &p);
    if let Some(parent) = p.parent() {
        fsync_dir(parent);
    }
}

/// Truncate/replace the whole session file with `messages` (used on reset /
/// compaction), re-writing the version header first. Atomic: writes a sibling
/// temp file, fsyncs it, then renames it over the target, so a crash mid-
/// rewrite never truncates the existing conversation — the old file stays intact
/// until the rename lands (P1-3: the old truncate-then-write lost everything on a
/// crash between truncate and final sync).
pub fn rewrite(path: &Path, messages: &[Message]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = crate::fsutil::unique_tmp(path);
    let Ok(mut f) = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
    else {
        eprintln!(
            "[session] rewrite open failed for {}; conversation not compacted on disk",
            path.display()
        );
        return;
    };
    // Preserve non-message journal kinds across compact/reset so
    // branch/compaction/delivery metadata is not wiped (CORE_REVIEW C5).
    let preserved_records: Vec<String> = std::fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|content| content.lines().map(str::to_string).collect::<Vec<_>>())
        .filter(|line| {
            serde_json::from_str::<Value>(line)
                .ok()
                .is_some_and(|value| {
                    value.get("_run").is_some()
                        || value.get("_compaction").is_some()
                        || value.get("_parent_delivery").is_some()
                        || value.get("_abandoned_branch").is_some()
                })
        })
        .collect();
    let _ = writeln!(f, "{}", header_line());
    // Rebuild a linear _entry chain so tree/branch ops still work after
    // compact/reset (CORE_REVIEW C5).
    let mut prev_id: Option<String> = None;
    let mut last_id: Option<String> = None;
    for m in messages {
        let id = next_entry_id();
        let entry = SessionEntry {
            id: id.clone(),
            parent_id: prev_id.clone(),
            message: Some(m.clone()),
        };
        let line = serde_json::json!({"_entry": entry});
        let _ = writeln!(f, "{line}");
        prev_id = Some(id.clone());
        last_id = Some(id);
    }
    for record in preserved_records {
        let _ = writeln!(f, "{record}");
    }
    let _ = f.flush();
    let _ = f.sync_all();
    drop(f); // release the handle before rename (Windows requires it)
             // Atomic on POSIX (same dir/same volume); best-effort on Windows.
    let _ = std::fs::rename(&tmp, path);
    if let Some(parent) = path.parent() {
        fsync_dir(parent);
    }
    if let Some(leaf) = last_id {
        let _ = set_active_leaf(path, &leaf);
    }
}

/// A lightweight description of a session file used by the session picker.
/// `title` is derived from the first user message so a session is identifiable
/// by its topic instead of by an opaque hex-hash filename. Because it is read
/// fresh from the append-only file each time `list_sessions` runs, it updates
/// automatically as the conversation grows (empty → first prompt → fuller).
pub struct SessionInfo {
    pub title: Option<String>,
    pub messages: usize,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: bool,
}

fn meta_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta.json", path.display()))
}

fn process_lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", path.display()))
}

fn meta_lock_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.meta.lock", path.display()))
}

pub fn read_meta(path: &Path) -> SessionMeta {
    std::fs::read(meta_path(path))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub fn update_meta(
    path: &Path,
    update: impl FnOnce(&mut SessionMeta),
) -> Result<SessionMeta, String> {
    let _lock =
        crate::fsutil::FileLock::acquire(&meta_lock_path(path)).map_err(|e| e.to_string())?;
    let mut meta = read_meta(path);
    update(&mut meta);
    let data = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
    crate::fsutil::atomic_write(&meta_path(path), &data).map_err(|e| e.to_string())?;
    Ok(meta)
}

pub fn delete_with_sidecars(path: &Path) -> Result<(), String> {
    if process_lock_path(path).exists() {
        return Err("session is active in another process".into());
    }
    std::fs::remove_file(path).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(meta_path(path));
    let _ = std::fs::remove_file(stats_path(path));
    let _ = std::fs::remove_file(meta_lock_path(path));
    Ok(())
}

/// Scan a session file once (streaming, bounded) and return its title + message
/// count. The title is the first user message's text, truncated to 80 chars.
/// Returns `title: None, messages: 0` for a missing or header-only file.
pub fn describe(path: &Path) -> SessionInfo {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return SessionInfo {
                title: None,
                messages: 0,
            }
        }
    };
    let reader = BufReader::new(file);
    let mut title: Option<String> = None;
    let mut messages = 0usize;
    let mut header_consumed = false;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if !header_consumed {
            header_consumed = true;
            // First non-empty line: if it's the version header, skip it (mirrors load()).
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v.get("_session_version").is_some() {
                    continue;
                }
            }
            // Old file with no header — this line is a real message; fall through.
        }
        let parsed_value = serde_json::from_str::<Value>(&line).ok();
        let parsed_message = parsed_value
            .as_ref()
            .and_then(|value| value.get("_entry"))
            .cloned()
            .and_then(|entry| {
                serde_json::from_value::<SessionEntry>(entry)
                    .ok()
                    .and_then(|entry| entry.message)
            })
            .or_else(|| {
                parsed_value.and_then(|value| serde_json::from_value::<Message>(value).ok())
            });
        let Some(msg) = parsed_message else {
            continue;
        };
        messages += 1;
        // Sanity guard against a pathological file; stop counting beyond this.
        if messages > 100_000 {
            break;
        }
        if title.is_none() {
            if let Some(t) = first_user_text(&msg) {
                let t: String = t.trim().chars().take(80).collect();
                title = Some(t);
            }
        }
    }
    SessionInfo { title, messages }
}

/// Extract the text of a user message: plain string content, or the joined
/// text parts of a multimodal content array. Returns None for non-user or
/// empty messages (so tool/assistant/system messages never become a title).
fn first_user_text(msg: &Message) -> Option<String> {
    if !msg.is_user() {
        return None;
    }
    if let Some(s) = msg.content_text() {
        if s.trim().is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    if let Some(parts) = msg.content_parts() {
        let mut out = String::new();
        for p in parts {
            if let crate::message::ContentPart::Text { text } = p {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        if out.trim().is_empty() {
            return None;
        }
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentPart, ImageUrl};

    #[test]
    fn append_then_load_roundtrip() {
        let dir = std::env::temp_dir().join("catalyst_code_session_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::system("x"));
        append(&p, &Message::user("hi"));
        let v = load(&p).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].role(), "system");
        // rewrite
        rewrite(&p, &[Message::system("y")]);
        assert_eq!(load(&p).unwrap().len(), 1);
    }

    #[test]
    fn append_flush_then_sync_persists() {
        let dir = std::env::temp_dir().join("catalyst_code_session_sync_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("one"));
        append(&p, &Message::user("two"));
        // Durability is deferred to sync — content must already be readable
        // after flush-only appends, and sync must be a no-op success.
        assert_eq!(load(&p).unwrap().len(), 2);
        sync(&p);
        assert_eq!(load(&p).unwrap().len(), 2);
    }

    #[test]
    fn header_version_is_present_and_validated() {
        let dir = std::env::temp_dir().join("catalyst_code_session_hdr_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("hi"));
        // header line present and well-formed
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.starts_with("{\"_session_version\": 2}"));
        // load() drops the header, returns only real messages
        assert_eq!(load(&p).unwrap().len(), 1);
    }

    #[test]
    fn future_version_refused() {
        let dir = std::env::temp_dir().join("catalyst_code_session_future");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(
            &p,
            "{\"_session_version\": 99}\n{\"role\":\"user\",\"content\":\"x\"}\n",
        )
        .unwrap();
        // refuses to load a newer-version file and returns a clear error
        let r = load(&p);
        assert!(r.is_err(), "expected an error for a future-version session");
        assert!(r.unwrap_err().contains("newer than supported"));
    }

    #[test]
    fn legacy_header_migrates_without_losing_messages() {
        let dir = std::env::temp_dir().join("catalyst_code_session_migration");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(
            &p,
            "{\"_session_version\": 1}\n{\"role\":\"user\",\"content\":\"old\"}\n",
        )
        .unwrap();
        let report = load_report(&p).unwrap();
        assert_eq!(report.messages.len(), 1);
        assert!(!report.warnings.is_empty());
        ensure(&p);
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .starts_with("{\"_session_version\": 2}"));
        assert_eq!(load(&p).unwrap().len(), 1);
    }

    #[test]
    fn recovery_reports_truncation_malformed_records_and_unfinished_runs() {
        let dir = std::env::temp_dir().join("catalyst_code_session_recovery");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"_session_version\": 2}\n",
                "{\"role\":\"user\",\"content\":\"survives\"}\n",
                "not-json\n",
                "{\"_run\":{\"session_id\":\"s1\",\"run_id\":\"r1\",\"state\":\"started\",\"timestamp_ms\":1}}\n",
                "{\"role\":\"assistant\",\"content\":"
            ),
        )
        .unwrap();
        let report = load_report(&p).unwrap();
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.unfinished_runs.len(), 1);
        assert_eq!(report.unfinished_runs[0].run_id, "r1");
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("malformed")));
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("truncated")));

        append_run_state(&p, "s1", "r1", RunState::Interrupted, Some("restart"));
        assert!(load_report(&p).unwrap().unfinished_runs.is_empty());
    }

    #[test]
    fn completed_run_is_not_reported_as_unfinished() {
        let dir = std::env::temp_dir().join("catalyst_code_session_run_terminal");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        ensure(&p);
        append_run_state(&p, "s1", "r1", RunState::Started, None);
        append_run_state(&p, "s1", "r1", RunState::Completed, None);
        assert!(load_report(&p).unwrap().unfinished_runs.is_empty());
    }

    #[test]
    fn recovery_identifies_interrupted_tool_subagent_and_goal_without_resuming() {
        let dir = std::env::temp_dir().join("catalyst_code_session_activity_recovery");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        ensure(&p);
        append_activity_state(
            &p,
            "s1",
            "tool-run",
            "tool",
            Some("parent-run"),
            Some("call-7"),
            RunState::Started,
            Some("bash"),
        );
        append_activity_state(
            &p,
            "s1",
            "sub-run",
            "subagent",
            Some("parent-run"),
            None,
            RunState::Started,
            Some("worker"),
        );
        append_activity_state(
            &p,
            "s1",
            "goal-1",
            "goal",
            None,
            None,
            RunState::Started,
            Some("deploying"),
        );

        let report = load_report(&p).unwrap();
        assert!(
            report.messages.is_empty(),
            "recovery must not synthesize replay work"
        );
        assert_eq!(report.unfinished_runs.len(), 3);
        let by_kind: std::collections::HashMap<_, _> = report
            .unfinished_runs
            .iter()
            .map(|record| (record.kind.as_deref().unwrap(), record))
            .collect();
        assert_eq!(by_kind["tool"].tool_call_id.as_deref(), Some("call-7"));
        assert_eq!(
            by_kind["subagent"].parent_run_id.as_deref(),
            Some("parent-run")
        );
        assert_eq!(by_kind["goal"].run_id, "goal-1");

        for record in report.unfinished_runs {
            append_activity_state(
                &p,
                &record.session_id,
                &record.run_id,
                record.kind.as_deref().unwrap_or("run"),
                record.parent_run_id.as_deref(),
                record.tool_call_id.as_deref(),
                RunState::Interrupted,
                Some("recovered after restart; not resumed"),
            );
        }
        assert!(load_report(&p).unwrap().unfinished_runs.is_empty());
    }

    #[test]
    fn rewrite_preserves_run_records_and_describe_excludes_them() {
        let dir = std::env::temp_dir().join("catalyst_code_session_rewrite_runs");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("hello"));
        append_run_state(&p, "s1", "r1", RunState::Started, None);
        assert_eq!(describe(&p).messages, 1);

        rewrite(&p, &[Message::user("compacted")]);

        let report = load_report(&p).unwrap();
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.unfinished_runs.len(), 1);
        assert_eq!(report.unfinished_runs[0].run_id, "r1");
        assert_eq!(describe(&p).messages, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn normal_replay_preserves_messages_and_terminal_activity() {
        let dir = std::env::temp_dir().join("catalyst_code_session_normal_replay");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("hello"));
        append(&p, &Message::assistant("done"));
        append_run_state(&p, "s1", "r1", RunState::Started, None);
        append_run_state(&p, "s1", "r1", RunState::Completed, None);
        let report = load_report(&p).unwrap();
        assert_eq!(report.messages.len(), 2);
        assert!(report.unfinished_runs.is_empty());
    }

    #[test]
    fn describe_extracts_first_user_title_and_count() {
        let dir = std::env::temp_dir().join("catalyst_code_session_describe_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::system("sys"));
        append(&p, &Message::user("Add a login form to the app"));
        append(&p, &Message::assistant("done"));
        append(&p, &Message::user("now add tests"));
        let info = describe(&p);
        assert_eq!(info.messages, 4);
        // Title is the FIRST user message (the stable topic), not the latest.
        assert_eq!(info.title.as_deref(), Some("Add a login form to the app"));
    }

    #[test]
    fn describe_header_only_and_multimodal() {
        let dir = std::env::temp_dir().join("catalyst_code_session_describe_mm");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        // header-only file: no messages, no title
        let info = describe(&p);
        assert_eq!(info.messages, 0);
        assert!(info.title.is_none());
        // a multimodal user message: title is the joined text parts
        append(
            &p,
            &Message::user_multimodal(vec![
                ContentPart::Text {
                    text: "describe this".into(),
                },
                ContentPart::Image {
                    image_url: ImageUrl {
                        url: "data:image/png;base64,AAAA".into(),
                        detail: None,
                    },
                },
            ]),
        );
        let info = describe(&p);
        assert_eq!(info.messages, 1);
        assert_eq!(info.title.as_deref(), Some("describe this"));
    }

    #[test]
    fn describe_truncates_long_titles() {
        let dir = std::env::temp_dir().join("catalyst_code_session_describe_long");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let long = "x".repeat(200);
        append(&p, &Message::user(long));
        let info = describe(&p);
        assert_eq!(info.title.as_deref().map(|s| s.len()), Some(80));
    }

    #[test]
    fn stats_roundtrip_survives_restart() {
        let dir = std::env::temp_dir().join("catalyst_code_session_stats_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        // No sidecar yet → all zeros.
        let s = load_stats(&p);
        assert_eq!(
            (s.tokens_in, s.tokens_out, s.cached_tokens, s.turns),
            (0, 0, 0, 0)
        );
        // Persist some cumulative usage.
        save_stats(
            &p,
            &SessionStats {
                tokens_in: 12345,
                tokens_out: 678,
                cached_tokens: 9000,
                turns: 7,
                compactions: 3,
            },
        );
        // “Reopen” → the sidecar restores the same totals.
        let s = load_stats(&p);
        assert_eq!(s.tokens_in, 12345);
        assert_eq!(s.tokens_out, 678);
        assert_eq!(s.cached_tokens, 9000);
        assert_eq!(s.turns, 7);
        assert_eq!(s.compactions, 3);
        // Garbage in the sidecar degrades to zeros (never panics).
        std::fs::write(stats_path(&p), "not json").unwrap();
        let s = load_stats(&p);
        assert_eq!(
            (s.tokens_in, s.tokens_out, s.cached_tokens, s.turns),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn metadata_roundtrip_and_delete_removes_sidecars() {
        let dir = std::env::temp_dir().join("catalyst_code_session_meta_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("hello"));
        save_stats(
            &p,
            &SessionStats {
                turns: 1,
                ..SessionStats::default()
            },
        );

        update_meta(&p, |meta| {
            meta.title = Some("Renamed conversation".into());
            meta.pinned = true;
        })
        .unwrap();
        let meta = read_meta(&p);
        assert_eq!(meta.title.as_deref(), Some("Renamed conversation"));
        assert!(meta.pinned);

        delete_with_sidecars(&p).unwrap();
        assert!(!p.exists());
        assert!(!meta_path(&p).exists());
        assert!(!stats_path(&p).exists());
    }

    #[test]
    fn delete_refuses_a_session_locked_by_another_process() {
        let dir = std::env::temp_dir().join("catalyst_code_session_locked_delete_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        append(&p, &Message::user("still active"));
        std::fs::write(process_lock_path(&p), "pid=123").unwrap();

        assert!(delete_with_sidecars(&p).is_err());
        assert!(p.exists());
        std::fs::remove_file(process_lock_path(&p)).unwrap();
        delete_with_sidecars(&p).unwrap();
        assert!(!p.exists());
    }
}

#[cfg(test)]
mod parent_delivery_tests {
    use super::*;

    #[test]
    fn parent_delivery_is_exactly_once_and_loadable() {
        let dir = std::env::temp_dir().join(format!("catalyst_delivery_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        append(&path, &Message::user("hello"));
        let artifact =
            write_job_artifact(&path, "child", Some("parent"), "completed", "done").unwrap();
        append_parent_delivery(&path, "parent", "child", &artifact).unwrap();
        append_parent_delivery(&path, "parent", "child", &artifact).unwrap();
        let report = load_report(&path).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.parent_deliveries.len(), 1);
        assert_eq!(report.parent_deliveries[0].run_id, "child");
        assert_eq!(report.parent_deliveries[0].parent_run_id, "parent");
        assert!(report.parent_deliveries[0]
            .artifact_path
            .to_string_lossy()
            .contains("artifacts"));
        let count = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|line| line.contains("_parent_delivery"))
            .count();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod compaction_journal_tests {
    use super::*;

    #[test]
    fn automatic_compaction_persists_metadata_and_shakes_artifacts() {
        let dir = std::env::temp_dir().join(format!(
            "catalyst_automatic_compaction_persistence_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        append(&path, &Message::user("old context"));

        let kept = write_job_artifact(&path, "auto-kept", None, "completed", "referenced").unwrap();
        write_job_artifact(&path, "auto-drop", None, "completed", "unreferenced").unwrap();
        let retained = std::collections::HashSet::from(["auto-kept".to_string()]);
        let artifacts: Vec<String> = retained
            .iter()
            .map(|id| format!("artifact://{id}.json"))
            .collect();

        // This is the production automatic-persistence sequence: append the
        // compacted snapshot and then remove artifacts no longer referenced by
        // the active conversation.
        append_compaction(
            &path,
            &[Message::assistant("automatic summary")],
            "automatic compaction",
            &artifacts,
        )
        .unwrap();
        assert_eq!(shake_artifacts(&path, &retained).unwrap(), 1);

        let report = load_report(&path).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.messages[0].content_text(), Some("automatic summary"));
        assert_eq!(report.compactions.len(), 1);
        assert_eq!(report.compactions[0].summary, "automatic compaction");
        assert_eq!(report.compactions[0].artifacts, artifacts);
        assert!(read_job_artifact(&path, "auto-kept").is_some());
        assert!(read_job_artifact(&path, "auto-drop").is_none());
        assert!(kept.exists());

        let tree = session_tree(&path);
        assert_eq!(tree["compactions"][0]["summary"], "automatic compaction");
        assert_eq!(tree["compactions"][0]["artifacts"][0], artifacts[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod entry_reload_tests {
    use super::*;

    #[test]
    fn appended_entry_message_reloads_without_warning() {
        let dir =
            std::env::temp_dir().join(format!("catalyst_entry_reload_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        append(&path, &Message::user("hello"));
        let report = load_report(&path).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.messages.len(), 1);
        assert_eq!(report.messages[0].content_text(), Some("hello"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod append_only_compaction_tests {
    use super::*;

    #[test]
    fn compact_snapshot_preserves_entry_journal() {
        let dir =
            std::env::temp_dir().join(format!("catalyst_append_compact_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        append(&path, &Message::user("before"));
        append_compaction(&path, &[Message::assistant("summary")], "manual", &[]).unwrap();
        let journal = std::fs::read_to_string(&path).unwrap();
        assert!(journal.contains("\"_entry\""));
        assert!(journal.contains("before"));
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content_text(), Some("summary"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod session_tree_tests {
    use super::*;

    #[test]
    fn tree_reports_ancestry_and_siblings_without_rewriting_journal() {
        let dir = std::env::temp_dir().join(format!("catalyst_tree_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        append(&path, &Message::user("root"));
        let root = active_leaf(&path).unwrap();
        append(&path, &Message::user("child-a"));
        let child_a = active_leaf(&path).unwrap();
        let branch = create_branch(&path, &root).unwrap();
        append(&path, &Message::user("child-b"));
        let child_b = active_leaf(&path).unwrap();
        let tree = session_tree(&path);
        assert_eq!(tree["leaf"], child_b);
        assert_eq!(tree["ancestry"], serde_json::json!([root, branch, child_b]));
        assert_eq!(tree["siblings"], serde_json::json!([]));
        assert!(tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == branch && entry["parentId"] == root));
        assert!(tree["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == child_a && entry["parentId"] == root));
        assert_eq!(load(&path).unwrap().len(), 2);
        assert!(std::fs::read_to_string(&path).unwrap().contains("child-a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_leaf_and_parent_links_are_reported_safely() {
        let dir = std::env::temp_dir().join(format!("catalyst_tree_bad_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        ensure(&path);
        std::fs::write(path.with_extension("leaf.json"), "not-json").unwrap();
        let tree = session_tree(&path);
        assert!(tree["warning"].as_str().is_some());
        std::fs::write(&path, format!("{}\n{{\"_entry\":{{\"id\":\"x\",\"parentId\":\"missing\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}}}\n", header_line())).unwrap();
        let report = load_report(&path).unwrap();
        assert!(report.warnings.iter().any(|w| w.contains("missing parent")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod artifact_shake_tests {
    use super::*;

    #[test]
    fn artifact_reference_parser_preserves_job_run_ids() {
        assert_eq!(
            artifact_reference_run_id("artifact://job-abc.json"),
            Some("job-abc")
        );
        assert_eq!(
            artifact_reference_run_id("artifact://nested/job-xyz.json"),
            Some("job-xyz")
        );
        assert_eq!(artifact_reference_run_id("not-an-artifact"), None);
    }

    #[test]
    fn shake_retains_compaction_referenced_artifact() {
        let dir =
            std::env::temp_dir().join(format!("catalyst_artifact_ref_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        append_compaction(&path, &[], "summary", &["artifact://job-keep.json".into()]).unwrap();
        write_job_artifact_record(
            &path,
            "job-keep",
            None,
            "task",
            "completed",
            0,
            1,
            None,
            "summary",
        )
        .unwrap();
        write_job_artifact_record(
            &path,
            "job-drop",
            None,
            "task",
            "completed",
            0,
            1,
            None,
            "summary",
        )
        .unwrap();
        assert_eq!(
            shake_artifacts(&path, &std::collections::HashSet::new()).unwrap(),
            1
        );
        assert!(read_job_artifact(&path, "job-keep").is_some());
        assert!(read_job_artifact(&path, "job-drop").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shake_removes_only_unreferenced_job_artifacts() {
        let dir =
            std::env::temp_dir().join(format!("catalyst_artifact_shake_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        write_job_artifact(&path, "keep", None, "completed", "a").unwrap();
        write_job_artifact(&path, "drop", None, "completed", "b").unwrap();
        let keep = std::collections::HashSet::from(["keep".to_string()]);
        assert_eq!(shake_artifacts(&path, &keep).unwrap(), 1);
        assert!(read_job_artifact(&path, "keep").is_some());
        assert!(read_job_artifact(&path, "drop").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Durable record for a completed steerable subagent. Records are kept in a
/// session-owned sidecar so they survive core restarts without claiming any
/// cross-machine coordination semantics.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ParkedAgentRecord {
    pub run_id: String,
    pub target: String,
    pub agent: String,
    pub parent_run_id: Option<String>,
    pub depth: u32,
    pub parked_at_ms: u64,
    pub expires_at_ms: u64,
    pub messages: Vec<Message>,
}

fn parked_agents_path(session_path: &Path) -> PathBuf {
    session_path.with_extension("parked-agents.json")
}

pub fn store_parked_agent(session_path: &Path, record: &ParkedAgentRecord) -> Result<(), String> {
    let path = parked_agents_path(session_path);
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
        .map_err(|e| format!("lock parked agents: {e}"))?;
    let mut records = load_parked_agents_unchecked(&path);
    records.retain(|item| item.run_id != record.run_id && item.target != record.target);
    records.push(record.clone());
    crate::fsutil::atomic_write_str(
        &path,
        &(serde_json::to_string(&records).map_err(|e| e.to_string())? + "\n"),
    )
    .map_err(|e| format!("write parked agents: {e}"))
}

fn load_parked_agents_unchecked(path: &Path) -> Vec<ParkedAgentRecord> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn load_parked_agents(session_path: &Path, now_ms: u64) -> Vec<ParkedAgentRecord> {
    let path = parked_agents_path(session_path);
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock")).ok();
    let all = load_parked_agents_unchecked(&path);
    let live: Vec<_> = all
        .into_iter()
        .filter(|record| record.expires_at_ms > now_ms)
        .collect();
    let _ = crate::fsutil::atomic_write_str(
        &path,
        &(serde_json::to_string(&live).unwrap_or_else(|_| "[]".into()) + "\n"),
    );
    live
}

pub fn remove_parked_agent(session_path: &Path, run_id: &str) -> Result<(), String> {
    let path = parked_agents_path(session_path);
    let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
        .map_err(|e| format!("lock parked agents: {e}"))?;
    let mut records = load_parked_agents_unchecked(&path);
    records.retain(|record| record.run_id != run_id);
    crate::fsutil::atomic_write_str(
        &path,
        &(serde_json::to_string(&records).map_err(|e| e.to_string())? + "\n"),
    )
    .map_err(|e| format!("write parked agents: {e}"))
}

#[cfg(test)]
mod parked_agent_tests {
    use super::*;

    #[test]
    fn parked_agents_survive_restart_and_expire() {
        let dir = std::env::temp_dir().join(format!("catalyst_parked_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let record = ParkedAgentRecord {
            run_id: "run-1".into(),
            target: "worker-1".into(),
            agent: "worker".into(),
            parent_run_id: None,
            depth: 0,
            parked_at_ms: 10,
            expires_at_ms: 20,
            messages: vec![Message::user("task"), Message::assistant("done")],
        };
        store_parked_agent(&path, &record).unwrap();
        assert_eq!(load_parked_agents(&path, 19).len(), 1);
        assert!(load_parked_agents(&path, 20).is_empty());
        assert!(load_parked_agents(&path, 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
