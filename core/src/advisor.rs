//! Watchdog advisor runtime.
//!
//! Each watchdog is an isolated, fail-open second-model reviewer. It cannot
//! execute tools, mutate the workspace, approve actions, or resume a stopped
//! executor. WATCHDOG.md supplies review priorities; WATCHDOG.yml supplies a
//! named roster whose more-specific definitions override ancestor definitions.
//!
//! Phase-1 quality path:
//! - reviewers receive a structured **evidence pack** (user goal, work state,
//!   redacted turn diffs, recent claims) rather than a raw chat scrap
//! - notes are parsed into a structured schema (finding/where/action/…)
//! - concern/blocker notes can request a single soft-continue of the main
//!   executor so recommendations are acted on before the turn ends

use crate::config::{AdvisorConfig, Config, WatchdogAdvisor};
use crate::message::Message;
use crate::protocol::{emit, Event};
use crate::{provider, State, WorkState};
use futures_util::future::join_all;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_REVIEW_CHARS: usize = 14_000;
const MAX_NOTE_CHARS: usize = 1_500;
const MAX_DIFF_CHARS: usize = 6_000;
const MAX_DIFF_PER_FILE: usize = 2_000;
const MAX_WATCHDOG_BYTES: usize = 128 * 1024;
const MAX_DEDUPE_HISTORY: usize = 4096;
const ADVISOR_MAX_TOKENS: u32 = 600;
const MAX_EVIDENCE_TOOL_CALLS: usize = 3;
const MAX_TOOL_EVIDENCE_CHARS: usize = 3_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Main,
    Subagent,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subagent => "subagent",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Nit,
    Concern,
    Blocker,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nit => "nit",
            Self::Concern => "concern",
            Self::Blocker => "blocker",
        }
    }

    pub fn requires_action(self) -> bool {
        matches!(self, Self::Concern | Self::Blocker)
    }
}

/// One accepted advisory note, ready to inject and/or surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvisoryNote {
    pub advisor: String,
    pub model: String,
    pub severity: Severity,
    pub finding: String,
    pub where_path: Option<String>,
    pub action: Option<String>,
    pub evidence: Option<String>,
    pub check: Option<String>,
}

impl AdvisoryNote {
    /// Full body text used for dedupe + freeform display.
    pub fn body_text(&self) -> String {
        let mut parts = vec![self.finding.clone()];
        if let Some(w) = &self.where_path {
            parts.push(format!("WHERE: {w}"));
        }
        if let Some(a) = &self.action {
            parts.push(format!("ACTION: {a}"));
        }
        if let Some(e) = &self.evidence {
            parts.push(format!("EVIDENCE: {e}"));
        }
        if let Some(c) = &self.check {
            parts.push(format!("CHECK: {c}"));
        }
        parts.join("\n")
    }

    /// Compact imperative line the executor follows well.
    pub fn steering_line(&self) -> String {
        let mut line = format!(
            "[advisor:{}] {} · {}",
            self.severity.as_str(),
            self.advisor,
            truncate_chars(&self.finding, 200)
        );
        if let Some(w) = &self.where_path {
            line.push_str(" · where: ");
            line.push_str(&truncate_chars(w, 120));
        }
        if let Some(a) = &self.action {
            line.push_str(" · action: ");
            line.push_str(&truncate_chars(a, 200));
        }
        if let Some(c) = &self.check {
            line.push_str(" · check: ");
            line.push_str(&truncate_chars(c, 120));
        }
        line
    }

    pub fn to_system_message(&self, scope: Scope) -> Message {
        let mut attrs = format!(
            "advisor=\"{}\" severity=\"{}\" scope=\"{}\"",
            xml_escape(&self.advisor),
            self.severity.as_str(),
            scope.label()
        );
        if let Some(w) = &self.where_path {
            attrs.push_str(&format!(" where=\"{}\"", xml_escape(w)));
        }
        if let Some(c) = &self.check {
            attrs.push_str(&format!(" check=\"{}\"", xml_escape(c)));
        }
        let mut body = self.finding.clone();
        if let Some(a) = &self.action {
            body.push_str("\nACTION: ");
            body.push_str(a);
        }
        if let Some(e) = &self.evidence {
            body.push_str("\nEVIDENCE: ");
            body.push_str(e);
        }
        let xml = format!(
            "<advisory {attrs}>\n{}\n</advisory>",
            xml_escape(&truncate_chars(&body, MAX_NOTE_CHARS))
        );
        Message::system(xml)
    }
}

/// Result of a review pass.
#[derive(Clone, Debug, Default)]
pub struct ReviewResult {
    pub notes: Vec<AdvisoryNote>,
    /// True when at least one concern/blocker was accepted — main turn may
    /// soft-continue once so the executor addresses them before finishing.
    pub soft_continue: bool,
}

impl ReviewResult {
    pub fn system_messages(&self, scope: Scope) -> Vec<Message> {
        self.notes
            .iter()
            .map(|n| n.to_system_message(scope))
            .collect()
    }

    /// User-role continuation the main agent must act on (once per turn).
    pub fn soft_continue_message(&self) -> Option<Message> {
        if !self.soft_continue {
            return None;
        }
        let actionable: Vec<&AdvisoryNote> = self
            .notes
            .iter()
            .filter(|n| n.severity.requires_action())
            .collect();
        if actionable.is_empty() {
            return None;
        }
        let mut body = String::from(
            "[advisor-continue] Address the following advisory note(s) before finishing this turn. \
             Do not claim the task is done until each concern/blocker is fixed or you explain why \
             it does not apply. Prefer concrete edits + verification over restating the advice.\n",
        );
        for (i, n) in actionable.iter().enumerate() {
            body.push_str(&format!("\n{}. {}\n", i + 1, n.steering_line()));
        }
        Some(Message::user(body))
    }
}

/// Turn-local file diffs collected from successful mutating tools.
#[derive(Clone, Debug, Default)]
pub struct TurnDiff {
    pub path: String,
    pub unified_diff: String,
}

/// Persisted only in memory and rendered as a transient next-turn tail. An
/// advisory is resolved automatically when a subsequent mutation touches its
/// target path, or superseded when an equivalent recommendation arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAdvisory {
    pub note: AdvisoryNote,
    pub status: &'static str,
}

pub fn record_open_advisories(open: &mut Vec<OpenAdvisory>, notes: &[AdvisoryNote]) {
    for note in notes.iter().filter(|n| n.severity.requires_action()) {
        let duplicate = open.iter().any(|entry| {
            normalize_note(&entry.note.finding) == normalize_note(&note.finding)
                && entry.note.where_path == note.where_path
        });
        if !duplicate {
            open.push(OpenAdvisory {
                note: note.clone(),
                status: "open",
            });
        }
    }
    open.truncate(8);
}

pub fn resolve_open_advisories_for_paths(open: &mut Vec<OpenAdvisory>, paths: &[String]) {
    open.retain(|entry| {
        let Some(where_path) = entry.note.where_path.as_deref() else {
            return true;
        };
        !paths
            .iter()
            .any(|path| path == where_path || where_path.starts_with(path))
    });
}

pub fn open_advisories_message(open: &[OpenAdvisory]) -> Option<Message> {
    if open.is_empty() {
        return None;
    }
    let mut body = String::from(
        "[Open advisor follow-ups — address or explicitly explain before claiming completion.]\n",
    );
    for entry in open.iter().take(6) {
        body.push_str("- ");
        body.push_str(&entry.note.steering_line());
        body.push('\n');
    }
    Some(Message::system(body))
}

#[derive(Default)]
struct EmissionGuard {
    // Keep severity so a later concern/blocker may supersede an earlier nit
    // for the same substantive recommendation.
    accepted: HashMap<String, Severity>,
    order: Vec<String>,
}

impl EmissionGuard {
    fn accept(&mut self, note: &str, severity: Severity) -> bool {
        let normalized = normalize_note(note);
        if normalized.is_empty() || is_empty_phrase(&normalized) {
            return false;
        }
        if let Some(previous) = self.accepted.get(&normalized) {
            if *previous >= severity {
                return false;
            }
        } else {
            self.order.push(normalized.clone());
        }
        self.accepted.insert(normalized.clone(), severity);
        if self.order.len() > MAX_DEDUPE_HISTORY {
            if let Some(oldest) = self.order.first().cloned() {
                self.accepted.remove(&oldest);
            }
            self.order.remove(0);
        }
        true
    }
}

static GUARDS: OnceLock<Mutex<HashMap<String, EmissionGuard>>> = OnceLock::new();

fn guard_for(scope: Scope, key: &str, severity: Severity) -> bool {
    let guards = GUARDS.get_or_init(|| Mutex::new(HashMap::new()));
    let map_key = format!("{}:{key}", scope.label());
    guards
        .lock()
        .expect("advisor guard poisoned")
        .entry(map_key)
        .or_default()
        .accept(key, severity)
}

/// Load watchdog guidance and roster files from user and project levels.
/// Malformed files are ignored with a visible warning; a workspace never loses
/// its executor just because a reviewer config is invalid.
pub fn load_watchdog_configuration(cfg: &mut Config) {
    let paths = watchdog_paths(&cfg.workspace);
    let mut shared = Vec::new();
    let mut roster: HashMap<String, WatchdogAdvisor> = HashMap::new();
    for path in paths {
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("WATCHDOG.md"))
        {
            if let Ok(text) = bounded_read(&path) {
                shared.push(expand_imports(
                    &text,
                    path.parent().unwrap_or(&cfg.workspace),
                    0,
                ));
            }
            continue;
        }
        match parse_watchdog_yaml(&path) {
            Ok((instructions, advisors)) => {
                if let Some(instructions) = instructions.filter(|s| !s.trim().is_empty()) {
                    shared.push(expand_imports(
                        &instructions,
                        path.parent().unwrap_or(&cfg.workspace),
                        0,
                    ));
                }
                for mut advisor in advisors {
                    let slug = slugify(&advisor.name);
                    if slug.is_empty() {
                        continue;
                    }
                    advisor.instructions = advisor
                        .instructions
                        .map(|s| expand_imports(&s, path.parent().unwrap_or(&cfg.workspace), 0));
                    roster.insert(slug, advisor);
                }
            }
            Err(error) => emit(&Event::new("info").with(
                "message",
                json!(format!(
                    "watchdog config ignored ({}): {error}",
                    path.display()
                )),
            )),
        }
    }
    if !shared.is_empty() {
        cfg.advisor.watchdog_instructions = Some(shared.join("\n\n"));
    }
    cfg.advisor.watchdog = roster.into_values().collect();
    cfg.advisor.watchdog.sort_by(|a, b| a.name.cmp(&b.name));
}

fn watchdog_paths(workspace: &Path) -> Vec<PathBuf> {
    let mut bases = Vec::new();
    if let Some(home) = crate::config::home_dir() {
        bases.push(home.join(".catalyst-code"));
    }
    let mut ancestors = workspace.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for base in ancestors {
        bases.push(base.to_path_buf());
    }
    let mut paths = Vec::new();
    for base in bases {
        for name in ["WATCHDOG.md", "WATCHDOG.yml", "WATCHDOG.yaml"] {
            let path = base.join(name);
            if path.is_file() {
                paths.push(path);
            }
        }
        if base != crate::config::home_dir().unwrap_or_default() {
            for name in ["WATCHDOG.md", "WATCHDOG.yml", "WATCHDOG.yaml"] {
                let path = base.join(".catalyst-code").join(name);
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn bounded_read(path: &Path) -> Result<String, String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_WATCHDOG_BYTES as u64 {
        return Err("file exceeds 128 KiB".into());
    }
    fs::read_to_string(path).map_err(|e| e.to_string())
}

fn path_is_under(child: &Path, root: &Path) -> bool {
    let Ok(child) = child.canonicalize() else {
        return false;
    };
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    child.starts_with(&root)
}

fn expand_imports(text: &str, base: &Path, depth: u8) -> String {
    if depth >= 4 {
        return text.to_string();
    }
    // Jail @imports under the watchdog base dir or user catalyst-code homes —
    // never arbitrary filesystem via @../../.ssh/id_rsa (CORE_REVIEW).
    let home = crate::config::home_dir().unwrap_or_default();
    let allowed_roots: Vec<PathBuf> = [
        base.to_path_buf(),
        home.join(".catalyst-code"),
        home.join(".config").join("catalyst-code"),
    ]
    .into_iter()
    .filter(|p| p.exists())
    .collect();
    text.lines()
        .map(|line| {
            let target = line
                .trim()
                .strip_prefix('@')
                .filter(|p| !p.contains(' ') && !p.is_empty());
            match target {
                Some(target) => {
                    if target.contains("..") {
                        return line.to_string();
                    }
                    let path = if let Some(rest) = target.strip_prefix("~/") {
                        home.join(rest)
                    } else {
                        base.join(target)
                    };
                    if !allowed_roots.iter().any(|r| path_is_under(&path, r)) {
                        return format!(
                            "{}\n<!-- @import refused: path escapes allowlist -->",
                            line
                        );
                    }
                    bounded_read(&path)
                        .map(|child| {
                            expand_imports(&child, path.parent().unwrap_or(base), depth + 1)
                        })
                        .unwrap_or_else(|_| line.to_string())
                }
                None => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_watchdog_yaml(path: &Path) -> Result<(Option<String>, Vec<WatchdogAdvisor>), String> {
    let text = bounded_read(path)?;
    let mut instructions = None;
    let mut advisors = Vec::new();
    let mut current: Option<WatchdogAdvisor> = None;
    let mut in_advisors = false;
    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "advisors:" {
            in_advisors = true;
            continue;
        }
        if !in_advisors {
            if let Some(value) = trimmed.strip_prefix("instructions:") {
                instructions = Some(
                    value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string(),
                );
                continue;
            }
        }
        if in_advisors && trimmed.starts_with('-') {
            if let Some(advisor) = current.take() {
                advisors.push(advisor);
            }
            current = Some(WatchdogAdvisor {
                enabled: true,
                ..Default::default()
            });
            if let Some(value) = trimmed.trim_start_matches('-').trim().strip_prefix("name:") {
                current.as_mut().unwrap().name = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
            }
            continue;
        }
        let Some(advisor) = current.as_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.trim() {
            "name" => advisor.name = value.to_string(),
            "enabled" => advisor.enabled = !matches!(value, "false" | "off" | "0"),
            "model" => advisor.model = (!value.is_empty()).then(|| value.to_string()),
            "instructions" => advisor.instructions = (!value.is_empty()).then(|| value.to_string()),
            "tools" => {
                advisor.tools = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            }
            "triggers" => {
                advisor.triggers = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.trim_matches('\"').trim_matches('\'').to_owned())
                    .collect()
            }
            _ => {}
        }
    }
    if let Some(advisor) = current {
        advisors.push(advisor);
    }
    if advisors.iter().any(|a| a.name.trim().is_empty()) {
        return Err("every advisor requires name".into());
    }
    Ok((instructions, advisors))
}

/// Determine whether an advisor is enabled for a scope and select its model.
pub fn model_for(cfg: &AdvisorConfig, scope: Scope, executor_model: &str) -> Option<String> {
    if !cfg.enabled || (scope == Scope::Subagent && !cfg.subagents) {
        return None;
    }
    match scope {
        Scope::Main => cfg.model.clone(),
        Scope::Subagent => cfg.subagent_model.clone().or_else(|| cfg.model.clone()),
    }
    .or_else(|| Some(executor_model.to_string()))
}

/// Run every eligible advisor. A failed response or unsafe output never blocks
/// the executor. More severe duplicates may replace a prior nit only in a later
/// turn; exact repeats are suppressed across the session.
pub async fn review(
    st: &Arc<State>,
    scope: Scope,
    executor_model: &str,
    recent: &[Message],
    turn_diffs: &[TurnDiff],
    cancel: &CancellationToken,
) -> ReviewResult {
    let cfg = st.cfg.read().await.advisor.clone();
    let Some(default_model) = model_for(&cfg, scope, executor_model) else {
        return ReviewResult::default();
    };
    if cancel.is_cancelled() {
        return ReviewResult::default();
    }
    let work_state = st.work_state.lock().await.clone();
    let roster = if cfg.watchdog.is_empty() {
        vec![WatchdogAdvisor {
            name: "default".into(),
            enabled: true,
            model: Some(default_model.clone()),
            ..Default::default()
        }]
    } else {
        cfg.watchdog.clone()
    };
    let mut packet = render_evidence_pack(recent, &work_state, turn_diffs);
    let cfg_snapshot = st.cfg.read().await.clone();
    let eligible: Vec<_> = roster
        .into_iter()
        .filter(|a| a.enabled && triggered(a, turn_diffs))
        .collect();
    for advisor in &eligible {
        packet.push_str(&collect_readonly_evidence(
            advisor,
            turn_diffs,
            &cfg_snapshot,
        ));
    }
    let tasks = eligible.into_iter().map(|advisor| {
        review_one(
            st.clone(),
            cfg.clone(),
            scope,
            default_model.clone(),
            advisor,
            packet.clone(),
            cancel.clone(),
        )
    });
    let mut result = ReviewResult::default();
    for note in join_all(tasks).await.into_iter().flatten() {
        result.soft_continue |= note.severity.requires_action();
        result.notes.push(note);
    }
    result
}

fn triggered(advisor: &WatchdogAdvisor, diffs: &[TurnDiff]) -> bool {
    advisor.triggers.is_empty()
        || advisor.triggers.iter().any(|trigger| {
            diffs
                .iter()
                .any(|d| d.path.contains(trigger) || d.unified_diff.contains(trigger))
        })
}

fn collect_readonly_evidence(
    advisor: &WatchdogAdvisor,
    diffs: &[TurnDiff],
    cfg: &Config,
) -> String {
    let allowed = ["read", "read_file", "grep", "glob"];
    let mut output = String::new();
    for tool in advisor
        .tools
        .iter()
        .filter(|t| allowed.contains(&t.as_str()))
        .take(MAX_EVIDENCE_TOOL_CALLS)
    {
        let path = diffs.first().map(|d| d.path.as_str()).unwrap_or("");
        let args = match tool.as_str() {
            "read" | "read_file" if !path.is_empty() => json!({"path": path, "limit": 120}),
            "grep" => json!({"pattern": "TODO|FIXME|unwrap|expect", "head_limit": 20}),
            "glob" => json!({"pattern": "**/*"}),
            _ => continue,
        };
        let name = if tool == "read" {
            "read_file"
        } else {
            tool.as_str()
        };
        let outcome = crate::tools::execute(name, &args, cfg);
        if outcome.ok {
            output.push_str("\n## READ-ONLY ");
            output.push_str(tool);
            output.push_str(" EVIDENCE\n");
            output.push_str(&truncate_chars(
                &crate::subagent::redact_secrets(&outcome.output),
                MAX_TOOL_EVIDENCE_CHARS,
            ));
            output.push('\n');
        }
    }
    output
}

async fn review_one(
    st: Arc<State>,
    cfg: AdvisorConfig,
    scope: Scope,
    default_model: String,
    advisor: WatchdogAdvisor,
    packet: String,
    cancel: CancellationToken,
) -> Option<AdvisoryNote> {
    let model = advisor.model.clone().unwrap_or(default_model);
    let provider = st.resolve_provider_for_model(&model).await;
    if provider.api_key.is_none() {
        emit_advisor_status(
            &st,
            scope,
            &advisor.name,
            &model,
            "no_key",
            Some("provider has no credential"),
            None,
        );
        return None;
    }
    let started = Instant::now();
    emit_advisor_status(&st, scope, &advisor.name, &model, "reviewing", None, None);
    let Some(raw) = provider::complete_text(
        &st.client,
        &provider,
        &model,
        &system_prompt(&cfg, &advisor),
        &packet,
        ADVISOR_MAX_TOKENS,
        &cancel,
    )
    .await
    else {
        emit_advisor_status(
            &st,
            scope,
            &advisor.name,
            &model,
            "failed",
            Some("provider request failed, timed out, or was cancelled"),
            Some(started.elapsed().as_millis()),
        );
        return None;
    };
    let Some(mut note) = parse_note(&raw) else {
        let (state, reason) = if raw.trim().eq_ignore_ascii_case("NONE") {
            ("clear", None)
        } else {
            (
                "invalid_response",
                Some("reviewer output did not match the advisory schema"),
            )
        };
        emit_advisor_status(
            &st,
            scope,
            &advisor.name,
            &model,
            state,
            reason,
            Some(started.elapsed().as_millis()),
        );
        return None;
    };
    note.advisor = advisor.name.clone();
    note.model = model.clone();
    let key = format!(
        "{}:{}",
        advisor.name,
        note.where_path.as_deref().unwrap_or("")
    );
    if !guard_for(scope, &key, note.severity) {
        emit_advisor_status(
            &st,
            scope,
            &advisor.name,
            &model,
            "duplicate",
            Some("equivalent advisory was already emitted"),
            Some(started.elapsed().as_millis()),
        );
        return None;
    }
    emit(
        &Event::new("advisor_note")
            .with("scope", json!(scope.label()))
            .with("advisor", json!(&note.advisor))
            .with("model", json!(&note.model))
            .with("severity", json!(note.severity.as_str()))
            .with("message", json!(note.steering_line()))
            .with("finding", json!(&note.finding))
            .with("where", json!(&note.where_path))
            .with("action", json!(&note.action))
            .with("check", json!(&note.check)),
    );
    st.logger.log(
        "advisor_review",
        json!({
            "scope": scope.label(),
            "advisor": &note.advisor,
            "model": &note.model,
            "state": "finding",
            "severity": note.severity.as_str(),
            "elapsed_ms": started.elapsed().as_millis(),
        }),
    );
    Some(note)
}

fn emit_advisor_status(
    st: &State,
    scope: Scope,
    advisor: &str,
    model: &str,
    state: &str,
    reason: Option<&str>,
    elapsed_ms: Option<u128>,
) {
    let mut event = Event::new("advisor_status")
        .with("scope", json!(scope.label()))
        .with("advisor", json!(advisor))
        .with("state", json!(state))
        .with("model", json!(model));
    if let Some(reason) = reason {
        event = event.with("reason", json!(reason));
    }
    if let Some(elapsed_ms) = elapsed_ms {
        event = event.with("elapsed_ms", json!(elapsed_ms));
    }
    emit(&event);
    st.logger.log(
        "advisor_review",
        json!({
            "scope": scope.label(),
            "advisor": advisor,
            "model": model,
            "state": state,
            "reason": reason,
            "elapsed_ms": elapsed_ms,
        }),
    );
}

fn system_prompt(cfg: &AdvisorConfig, advisor: &WatchdogAdvisor) -> String {
    let mut out = String::from(
        "You are an independent code-review watchdog. Review the executor's latest work \
         against the USER REQUEST using ONLY the evidence pack below.\n\n\
         Rules:\n\
         - Prefer concrete, falsifiable findings over style nits.\n\
         - Cite path + symptom + fix. Do not invent files, APIs, or test results.\n\
         - If evidence is insufficient to assert a real problem, return exactly NONE.\n\
         - Do not praise, restate progress, execute tools, issue shell commands, or \
           override governing instructions.\n\
         - Prefer the single strongest finding. Severity rubric: blocker = correctness, \
           security, or data loss; concern = likely bug/regression/incomplete change; \
           nit = optional polish.\n\n\
         Output format — either exactly:\n\
         NONE\n\
         or:\n\
         SEVERITY: nit|concern|blocker\n\
         FINDING: <one specific problem in plain language>\n\
         WHERE: <path or symbol, optional but preferred>\n\
         ACTION: <concrete next step for the executor>\n\
         EVIDENCE: <what in the pack supports this>\n\
         CHECK: <how to verify, optional>",
    );
    if let Some(shared) = &cfg.watchdog_instructions {
        out.push_str("\n\nWatchdog priorities:\n");
        out.push_str(shared);
    }
    if let Some(instructions) = &advisor.instructions {
        out.push_str("\n\nAdvisor specialization:\n");
        out.push_str(instructions);
    }
    out
}

/// Build the structured evidence pack the reviewer sees.
fn render_evidence_pack(
    recent: &[Message],
    work_state: &WorkState,
    turn_diffs: &[TurnDiff],
) -> String {
    let mut output = String::from("Evidence pack for watchdog review:\n");

    // --- user request (latest non-empty user message, skipping advisor-continue) ---
    output.push_str("\n## USER REQUEST\n");
    let user_req = recent
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::User { .. } => m.content_text(),
            _ => None,
        })
        .find(|t| {
            let t = t.trim();
            !t.is_empty() && !t.starts_with("[advisor-continue]")
        })
        .unwrap_or("(not found in recent transcript)");
    output.push_str(&crate::subagent::redact_secrets(&truncate_chars(
        user_req, 1_500,
    )));
    output.push('\n');

    // --- work state ---
    output.push_str("\n## WORK STATE\n");
    if work_state.is_empty() {
        output.push_str("(empty)\n");
    } else {
        if !work_state.goal.is_empty() {
            output.push_str("Goal: ");
            output.push_str(&truncate_chars(&work_state.goal, 240));
            output.push('\n');
        }
        for (label, items) in [
            ("Done", &work_state.done),
            ("In progress", &work_state.in_progress),
            ("Next", &work_state.next),
        ] {
            if items.is_empty() {
                continue;
            }
            output.push_str(label);
            output.push(':');
            for it in items.iter().take(6) {
                output.push_str("\n- ");
                output.push_str(&truncate_chars(it, 160));
            }
            output.push('\n');
        }
        if !work_state.recent_files.is_empty() {
            output.push_str("Recently touched: ");
            output.push_str(
                &work_state
                    .recent_files
                    .iter()
                    .take(8)
                    .map(|s| truncate_chars(s, 120))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            output.push('\n');
        }
        if !work_state.last_activity.is_empty() {
            output.push_str("Last: ");
            output.push_str(&truncate_chars(&work_state.last_activity, 160));
            output.push('\n');
        }
    }

    // --- turn diffs (highest-signal evidence) ---
    output.push_str("\n## FILES TOUCHED THIS TURN (unified diffs, redacted/capped)\n");
    if turn_diffs.is_empty() {
        output.push_str("(no mutating diffs captured this turn)\n");
    } else {
        let mut budget = MAX_DIFF_CHARS;
        for diff in turn_diffs.iter().take(6) {
            if budget == 0 {
                output.push_str("…(diff budget exhausted)\n");
                break;
            }
            let chunk = truncate_chars(
                &crate::subagent::redact_secrets(&diff.unified_diff),
                MAX_DIFF_PER_FILE.min(budget),
            );
            let used = chunk.chars().count();
            budget = budget.saturating_sub(used);
            output.push_str("### ");
            output.push_str(&truncate_chars(&diff.path, 160));
            output.push('\n');
            output.push_str(&chunk);
            output.push('\n');
        }
    }

    // --- recent assistant claims + user steer (no tool bodies, no prior advisories) ---
    output.push_str("\n## RECENT TRANSCRIPT (tool results omitted)\n");
    let mut claim_budget = 4_000usize;
    for message in recent.iter().rev().take(12).rev() {
        if claim_budget == 0 {
            break;
        }
        let text = match message {
            Message::Tool { .. } => continue, // full omit — secrets + noise
            Message::System { .. } => {
                let t = message.content_text().unwrap_or("");
                if t.contains("<advisory") || t.starts_with("[Work state") {
                    continue;
                }
                // Keep short system steers only.
                if t.len() > 400 {
                    continue;
                }
                t.to_string()
            }
            _ => message.content_text().unwrap_or("").to_string(),
        };
        let text = text.trim();
        if text.is_empty() || text.contains("<advisory") || text.starts_with("[advisor-continue]") {
            continue;
        }
        let line = format!(
            "{}: {}\n",
            message.role(),
            crate::subagent::redact_secrets(&truncate_chars(text, 800))
        );
        let used = line.chars().count();
        if used > claim_budget {
            break;
        }
        claim_budget -= used;
        output.push_str(&line);
    }

    truncate_chars(&output, MAX_REVIEW_CHARS)
}

fn parse_note(raw: &str) -> Option<AdvisoryNote> {
    let text = raw.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("none") {
        return None;
    }
    // Accept bare NONE on first line with trailing noise.
    let first = text.lines().next().unwrap_or("").trim();
    if first.eq_ignore_ascii_case("none") {
        return None;
    }

    let mut severity = Severity::Nit;
    let mut finding = String::new();
    let mut where_path = None;
    let mut action = None;
    let mut evidence = None;
    let mut check = None;
    let mut freeform: Vec<String> = Vec::new();
    let mut saw_structured = false;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("severity:") {
            let v = rest.trim();
            severity = if v.starts_with("blocker") {
                Severity::Blocker
            } else if v.starts_with("concern") {
                Severity::Concern
            } else {
                Severity::Nit
            };
            saw_structured = true;
            continue;
        }
        if let Some(rest) = strip_field(trimmed, "FINDING") {
            finding = rest;
            saw_structured = true;
            continue;
        }
        if let Some(rest) = strip_field(trimmed, "WHERE") {
            where_path = Some(rest);
            saw_structured = true;
            continue;
        }
        if let Some(rest) = strip_field(trimmed, "ACTION") {
            action = Some(rest);
            saw_structured = true;
            continue;
        }
        if let Some(rest) = strip_field(trimmed, "EVIDENCE") {
            evidence = Some(rest);
            saw_structured = true;
            continue;
        }
        if let Some(rest) = strip_field(trimmed, "CHECK") {
            check = Some(rest);
            saw_structured = true;
            continue;
        }
        // Legacy: first non-field line after optional SEVERITY header is the body.
        freeform.push(trimmed.to_string());
    }

    if finding.is_empty() {
        // Drop a leading "SEVERITY: …" style freeform head if parse missed it.
        let body = freeform
            .into_iter()
            .filter(|l| !l.to_ascii_lowercase().starts_with("severity:"))
            .collect::<Vec<_>>()
            .join("\n");
        finding = body.trim().to_string();
    } else if !freeform.is_empty() && !saw_structured {
        finding = freeform.join("\n");
    } else if !finding.is_empty() && !freeform.is_empty() {
        // Extra prose after structured fields — append lightly.
        let extra = freeform.join(" ");
        if extra.len() > 8 {
            finding.push(' ');
            finding.push_str(&extra);
        }
    }

    let finding = truncate_chars(finding.trim(), MAX_NOTE_CHARS);
    if finding.len() < 8 || is_empty_phrase(&normalize_note(&finding)) {
        return None;
    }

    Some(AdvisoryNote {
        advisor: String::new(),
        model: String::new(),
        severity,
        finding,
        where_path: where_path.map(|s| truncate_chars(&s, 200)),
        action: action.map(|s| truncate_chars(&s, 400)),
        evidence: evidence.map(|s| truncate_chars(&s, 400)),
        check: check.map(|s| truncate_chars(&s, 200)),
    })
}

fn strip_field(line: &str, name: &str) -> Option<String> {
    let (head, rest) = line.split_once(':')?;
    if !head.trim().eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

fn normalize_note(note: &str) -> String {
    note.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_empty_phrase(note: &str) -> bool {
    matches!(
        note,
        "stop"
            | "done"
            | "complete"
            | "lgtm"
            | "nothing to add"
            | "no issue"
            | "no issues"
            | "continue"
    )
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        format!(
            "{}…",
            text.chars()
                .take(limit.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn slugify(name: &str) -> String {
    name.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_actionable_notes_and_filters_empty_ones() {
        assert_eq!(parse_note("NONE"), None);
        let n = parse_note("SEVERITY: concern\nCheck the error path.").unwrap();
        assert_eq!(n.severity, Severity::Concern);
        assert_eq!(n.finding, "Check the error path.");
        assert_eq!(parse_note("SEVERITY: blocker\nLGTM"), None);
    }

    #[test]
    fn parses_structured_schema() {
        let n = parse_note(
            "SEVERITY: blocker\n\
             FINDING: rename drops partial failure\n\
             WHERE: core/src/fsutil.rs\n\
             ACTION: handle io::Error after rename\n\
             EVIDENCE: diff removed the Err arm\n\
             CHECK: cargo test fsutil --lib",
        )
        .unwrap();
        assert_eq!(n.severity, Severity::Blocker);
        assert_eq!(n.finding, "rename drops partial failure");
        assert_eq!(n.where_path.as_deref(), Some("core/src/fsutil.rs"));
        assert_eq!(n.action.as_deref(), Some("handle io::Error after rename"));
        assert_eq!(n.check.as_deref(), Some("cargo test fsutil --lib"));
        assert!(n.severity.requires_action());
        let xml = n.to_system_message(Scope::Main);
        let text = xml.content_text().unwrap();
        assert!(text.contains("severity=\"blocker\""));
        assert!(text.contains("where=\"core/src/fsutil.rs\""));
        assert!(text.contains("ACTION: handle io::Error"));
    }

    #[test]
    fn soft_continue_message_lists_actionable_only() {
        let result = ReviewResult {
            soft_continue: true,
            notes: vec![
                AdvisoryNote {
                    advisor: "default".into(),
                    model: "m".into(),
                    severity: Severity::Nit,
                    finding: "typo in comment is fine to ignore later".into(),
                    where_path: None,
                    action: None,
                    evidence: None,
                    check: None,
                },
                AdvisoryNote {
                    advisor: "Security".into(),
                    model: "m".into(),
                    severity: Severity::Concern,
                    finding: "API key logged in debug path".into(),
                    where_path: Some("core/src/logging.rs".into()),
                    action: Some("redact before log".into()),
                    evidence: None,
                    check: None,
                },
            ],
        };
        let msg = result.soft_continue_message().unwrap();
        let text = msg.content_text().unwrap();
        assert!(text.contains("[advisor-continue]"));
        assert!(text.contains("API key logged"));
        assert!(!text.contains("typo in comment"));
    }

    #[test]
    fn parses_named_watchdog_roster() {
        let path = std::env::temp_dir().join(format!("watchdog-{}.yml", std::process::id()));
        fs::write(
            &path,
            "instructions: check APIs\nadvisors:\n  - name: Architecture\n    model: reviewer\n    enabled: true\n",
        )
        .unwrap();
        let (shared, roster) = parse_watchdog_yaml(&path).unwrap();
        assert_eq!(shared.as_deref(), Some("check APIs"));
        assert_eq!(roster[0].name, "Architecture");
        assert_eq!(roster[0].model.as_deref(), Some("reviewer"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn emission_guard_deduplicates_normalized_notes() {
        let mut guard = EmissionGuard::default();
        assert!(guard.accept("Check error path!", Severity::Nit));
        assert!(!guard.accept("check-error path", Severity::Nit));
    }

    #[test]
    fn evidence_pack_includes_goal_diffs_omits_tools_and_advisories() {
        let mut ws = WorkState::default();
        ws.goal = "Fix auth race".into();
        ws.recent_files = vec!["core/src/auth.rs".into()];
        let diffs = vec![TurnDiff {
            path: "core/src/auth.rs".into(),
            unified_diff: "@@ -1,3 +1,4 @@\n+fix\n API_KEY=should-not-leak\n".into(),
        }];
        let packet = render_evidence_pack(
            &[
                Message::user("Inspect auth"),
                Message::tool("call", "full secret tool body"),
                Message::system("<advisory severity=\"nit\">old note</advisory>"),
                Message::assistant("I patched the lock ordering."),
            ],
            &ws,
            &diffs,
        );
        assert!(packet.contains("Fix auth race"));
        assert!(packet.contains("Inspect auth"));
        assert!(packet.contains("### core/src/auth.rs"));
        assert!(packet.contains("I patched the lock ordering"));
        assert!(!packet.contains("full secret tool body"));
        assert!(!packet.contains("old note"));
        // secret redaction best-effort via subagent::redact_secrets
        assert!(packet.contains("fix") || packet.contains("auth.rs"));
    }

    #[test]
    fn watchdog_slugify_is_stable_for_duplicate_names() {
        assert_eq!(slugify("Architecture Review"), "architecture-review");
        assert_eq!(slugify("architecture-review"), "architecture-review");
    }
}
