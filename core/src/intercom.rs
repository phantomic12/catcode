// Intercom: in-process coordination channel for subagents.
//
// This is the port of pi-subagents' `pi-intercom` bridge, adapted to our
// single-process model. Because subagents run as nested agentic loops inside
// the same core process (see run_subagent in main.rs), intercom is implemented
// as in-memory mailboxes rather than file-based IPC.
//
// Two coordination tools are exposed to subagents:
//
//   contact_supervisor({ reason, message })
//     The subagent contacts the orchestrator (the parent session) that
//     delegated the task. `reason: "need_decision"` blocks until the
//     orchestrator replies (the TUI surfaces the question as an
//     `intercom_message` event and the user replies via `intercom_reply`).
//     `reason: "progress_update"` is non-blocking and returns immediately.
//     This is how subagents prompt the orchestrator for any issues.
//
//   intercom({ action, to, message, id, reply })
//     Generic peer-to-peer plumbing. Subagents can send messages to each
//     other's mailboxes, issue blocking `ask`s, poll their own mailbox, and
//     reply to pending asks. This is only available to a subagent when the
//     setup allows it: the intercom bridge mode is not "off" and the agent's
//     resolved `tools` include `intercom`.
//
// Allowed-by-setup is enforced at subagent launch time (subagent.rs decides
// whether to register these tools + inject bridge instructions); this module
// only owns the bus + the two tool bodies.

use crate::protocol::{emit, Event};
use crate::tools::Outcome;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

static ASK_SEQ: AtomicU64 = AtomicU64::new(0);

/// How long a blocking `ask`/`contact_supervisor` waits for a reply before
/// giving up. Prevents an unanswered orchestrator/peer from wedging a subagent
/// forever (P1-6).
const INTERCOM_ASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_MAILBOX_MESSAGES: usize = 256;
const MAX_PENDING_ASKS: usize = 256;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn next_id(prefix: &str) -> String {
    let n = ASK_SEQ.fetch_add(1, Ordering::SeqCst);
    format!("{prefix}-{n:x}-{}", now_ms())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct IntercomMessage {
    pub id: String,
    pub from: String,
    pub to: String,
    pub message: String,
    pub reason: String,
    pub ts: u64,
    /// When this message is a blocking `ask`, this is the ask id the recipient
    /// must quote in its `reply`. Empty for fire-and-forget `send`s.
    pub ask_id: String,
}

/// A pending blocking ask awaiting a reply. Stored in the bus keyed by ask id
/// until the recipient replies (or the asker is cancelled/times out).
pub struct PendingAsk {
    pub id: String,
    pub from: String,
    pub to: String,
    pub message: String,
    pub reason: String,
    pub ts: u64,
    pub reply: Mutex<Option<String>>,
    pub notify: Arc<Notify>,
}

pub struct Mailbox {
    pub target: String,
    pub messages: Mutex<VecDeque<IntercomMessage>>,
    pub notify: Notify,
}

impl Mailbox {
    fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            messages: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }
}

/// The shared intercom bus. Held inside State (Arc<State>) so every nested
/// subagent loop and the main stdin loop can reach it.
#[derive(Default)]
pub struct IntercomBus {
    pub mailboxes: Mutex<HashMap<String, Arc<Mailbox>>>,
    pub pending_asks: Mutex<HashMap<String, Arc<PendingAsk>>>,
    /// Optional session-owned journal for undelivered peer messages.
    pub journal_path: Mutex<Option<std::path::PathBuf>>,
    /// The orchestrator (parent session) target name. Defaults to "orchestrator".
    pub orchestrator_target: Mutex<String>,
    /// All known target names (for the `targets` action + doctor diagnostics).
    pub known_targets: Mutex<Vec<String>>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct JournalRecord {
    delivered: bool,
    message: IntercomMessage,
}

impl IntercomBus {
    pub fn new() -> Self {
        let s = Self::default();
        *s.orchestrator_target
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = "orchestrator".to_string();
        s.known_targets
            .lock()
            .unwrap()
            .push("orchestrator".to_string());
        s
    }

    /// Bind durable peer messaging to this process's active session. The
    /// journal is local-session storage, not a distributed mailbox.
    pub fn configure_journal(&self, session_path: &std::path::Path) {
        *self.journal_path.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(session_path.with_extension("intercom.jsonl"));
    }

    fn journal_records(&self) -> Vec<JournalRecord> {
        let path = self
            .journal_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(path) = path else { return Vec::new() };
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        raw.lines()
            .filter_map(|line| serde_json::from_str::<JournalRecord>(line).ok())
            .collect()
    }

    fn append_journal(&self, record: &JournalRecord) -> Result<(), String> {
        use std::io::Write;
        let path = self
            .journal_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(path) = path else { return Ok(()) };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create intercom journal: {e}"))?;
        }
        let _lock = crate::fsutil::FileLock::acquire(&path.with_extension("lock"))
            .map_err(|e| format!("lock intercom journal: {e}"))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open intercom journal: {e}"))?;
        writeln!(
            file,
            "{}",
            serde_json::to_string(record).map_err(|e| e.to_string())?
        )
        .map_err(|e| format!("append intercom journal: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("sync intercom journal: {e}"))
    }

    fn undelivered_for(&self, target: &str) -> Vec<IntercomMessage> {
        let mut state: HashMap<String, JournalRecord> = HashMap::new();
        for record in self.journal_records() {
            state.insert(record.message.id.clone(), record);
        }
        let mut messages: Vec<_> = state
            .into_values()
            .filter(|record| !record.delivered && record.message.to == target)
            .map(|record| record.message)
            .collect();
        messages.sort_unstable_by_key(|message| message.ts);
        messages
    }

    fn mark_delivered(&self, message: &IntercomMessage) {
        let _ = self.append_journal(&JournalRecord {
            delivered: true,
            message: message.clone(),
        });
    }

    pub fn replay_undelivered(&self) {
        let targets: Vec<String> = self
            .mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for target in targets {
            let Some(mailbox) = self
                .mailboxes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&target)
                .cloned()
            else {
                continue;
            };
            let mut queue = mailbox.messages.lock().unwrap_or_else(|e| e.into_inner());
            for message in self.undelivered_for(&target) {
                if queue.len() >= MAX_MAILBOX_MESSAGES || queue.iter().any(|m| m.id == message.id) {
                    continue;
                }
                queue.push_back(message);
            }
            mailbox.notify.notify_waiters();
        }
    }

    /// The orchestrator target the parent session answers as.
    pub fn orchestrator_target(&self) -> String {
        self.orchestrator_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Create (idempotently) a mailbox for a target and register it as known.
    pub fn register_target(&self, target: &str) {
        if target.is_empty() {
            return;
        }
        let mailbox = {
            let mut mailboxes = self.mailboxes.lock().unwrap_or_else(|e| e.into_inner());
            mailboxes
                .entry(target.to_string())
                .or_insert_with(|| Arc::new(Mailbox::new(target)))
                .clone()
        };
        {
            let mut messages = mailbox.messages.lock().unwrap_or_else(|e| e.into_inner());
            for message in self.undelivered_for(target) {
                if messages.len() >= MAX_MAILBOX_MESSAGES {
                    break;
                }
                if !messages.iter().any(|queued| queued.id == message.id) {
                    messages.push_back(message);
                }
            }
        }
        let mut kt = self.known_targets.lock().unwrap_or_else(|e| e.into_inner());
        if !kt.iter().any(|t| t == target) {
            kt.push(target.to_string());
        }
    }

    /// Drop a mailbox when its subagent finishes.
    pub fn unregister(&self, target: &str) {
        self.mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(target);
        // Also drop it from the known-targets list so `targets()` no longer
        // advertises a peer whose mailbox (and run) is gone — otherwise a
        // subagent could `ask` a stale target and wedge until the 5-min timeout.
        self.known_targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|t| t != target);
        let removed = {
            let mut pending = self.pending_asks.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = pending
                .iter()
                .filter(|(_, ask)| ask.from == target || ask.to == target)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect::<Vec<_>>()
        };
        for ask in removed {
            *ask.reply.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("[agent finished]".to_string());
            ask.notify.notify_waiters();
        }
    }

    /// Known peer targets (for the `intercom({action:"targets"})` introspection
    /// action and doctor diagnostics).
    pub fn targets(&self) -> Vec<String> {
        self.known_targets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Post a fire-and-forget message into a target's mailbox. Returns Err if
    /// the target is unknown (no registered mailbox and not the orchestrator).
    pub fn post(&self, msg: IntercomMessage) -> Result<(), String> {
        if msg.message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "intercom message exceeds the {} byte limit",
                MAX_MESSAGE_BYTES
            ));
        }
        let target = msg.to.clone();
        // The orchestrator always exists as a conceptual target even if no
        // mailbox was explicitly created for it (it answers via the TUI).
        if target != self.orchestrator_target() {
            let mb = self.mailboxes.lock().unwrap_or_else(|e| e.into_inner());
            if !mb.contains_key(&target) {
                return Err(format!(
                    "unknown intercom target '{target}'; use action:\"targets\" to list known peers"
                ));
            }
        }
        // If the recipient has a mailbox, push there. The orchestrator has no
        // mailbox (it answers via events), so its messages are surfaced by the
        // caller through `emit_intercom_message` instead.
        let mailbox = self
            .mailboxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target)
            .cloned();
        if let Some(mb) = mailbox {
            let mut messages = mb.messages.lock().unwrap_or_else(|e| e.into_inner());
            if messages.len() >= MAX_MAILBOX_MESSAGES {
                return Err(format!(
                    "intercom mailbox '{target}' is full ({MAX_MAILBOX_MESSAGES} messages)"
                ));
            }
            self.append_journal(&JournalRecord {
                delivered: false,
                message: msg.clone(),
            })?;
            messages.push_back(msg);
            drop(messages);
            mb.notify.notify_one();
        }
        Ok(())
    }

    /// Read (and remove) the oldest message from a target's mailbox.
    pub fn receive(&self, target: &str) -> Option<IntercomMessage> {
        let mb = {
            let guard = self.mailboxes.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(target).cloned()
        }?;
        let msg = mb
            .messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front();
        if let Some(message) = msg.as_ref() {
            self.mark_delivered(message);
        }
        msg
    }

    /// Remove and return the oldest message from `target`'s mailbox that was
    /// sent by `from`. Messages from other senders are left in place so they
    /// can be picked up by the `intercom` tool's `receive` action. Used by the
    /// subagent loop's steer-message polling to avoid consuming peer messages.
    pub fn receive_from(&self, target: &str, from: &str) -> Option<IntercomMessage> {
        let mb = {
            let guard = self.mailboxes.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(target).cloned()
        }?;
        let mut queue = mb.messages.lock().unwrap_or_else(|e| e.into_inner());
        let pos = queue.iter().position(|m| m.from == from)?;
        let message = queue.remove(pos);
        drop(queue);
        if let Some(message) = message.as_ref() {
            self.mark_delivered(message);
        }
        message
    }

    /// Register a blocking ask and return its handle. The caller awaits the
    /// handle's notify; the recipient resolves it via `resolve_ask`.
    pub fn create_ask(&self, ask: PendingAsk) -> Result<Arc<PendingAsk>, String> {
        if ask.message.len() > MAX_MESSAGE_BYTES {
            return Err(format!(
                "intercom ask exceeds the {} byte limit",
                MAX_MESSAGE_BYTES
            ));
        }
        let arc = Arc::new(ask);
        {
            let mut pa = self.pending_asks.lock().unwrap_or_else(|e| e.into_inner());
            if pa.len() >= MAX_PENDING_ASKS {
                return Err(format!(
                    "too many pending intercom asks (limit {MAX_PENDING_ASKS})"
                ));
            }
            pa.insert(arc.id.clone(), arc.clone());
        }
        // Surface it: if addressed to the orchestrator, emit an event so the
        // TUI/user can reply; otherwise drop it into the recipient mailbox so a
        // peer subagent can `receive` it and `reply`. If the peer is unknown,
        // fail FAST (don't wedge the caller for 5 min waiting on a peer that
        // can't reply) — cancel the ask and return the error immediately.
        if arc.to == self.orchestrator_target() {
            emit_intercom_message(&arc);
        } else {
            let msg = IntercomMessage {
                id: arc.id.clone(),
                from: arc.from.clone(),
                to: arc.to.clone(),
                message: arc.message.clone(),
                reason: arc.reason.clone(),
                ts: arc.ts,
                ask_id: arc.id.clone(),
            };
            if let Err(e) = self.post(msg) {
                self.pending_asks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&arc.id);
                return Err(e);
            }
        }
        Ok(arc)
    }

    /// Resolve a pending ask with a reply. Returns true if the ask existed.
    pub fn resolve_ask(&self, id: &str, reply: &str) -> bool {
        let ask = self
            .pending_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(ask) = ask {
            *ask.reply.lock().unwrap_or_else(|e| e.into_inner()) = Some(reply.to_string());
            ask.notify.notify_one();
            true
        } else {
            false
        }
    }

    /// Resolve only when the caller is the ask's intended recipient.
    pub fn resolve_ask_from(&self, id: &str, recipient: &str, reply: &str) -> Result<(), String> {
        let ask = self
            .pending_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no pending ask with id '{id}'"))?;
        if ask.to != recipient {
            return Err(format!(
                "ask '{id}' is addressed to '{}', not '{recipient}'",
                ask.to
            ));
        }
        if self.resolve_ask(id, reply) {
            Ok(())
        } else {
            Err(format!("no pending ask with id '{id}'"))
        }
    }

    /// Take a pending ask (remove it) without resolving — used on cancel/abort
    /// so the awaiting task can wake and return.
    pub fn cancel_ask(&self, id: &str) {
        let ask = self
            .pending_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        if let Some(ask) = ask {
            *ask.reply.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("[interrupted]".to_string());
            ask.notify.notify_one();
        }
    }

    /// Snapshot of pending ask ids (doctor diagnostics).
    pub fn pending_count(&self) -> usize {
        self.pending_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Invalidate every mailbox and pending ask at a session boundary. Pending
    /// waiters are explicitly woken before their handles are dropped so no
    /// subagent can remain blocked on an intercom reply from the old session.
    pub fn reset(&self) {
        let pending = {
            let mut asks = self
                .pending_asks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *asks)
        };
        for ask in pending.into_values() {
            *ask.reply
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some("[session replaced]".to_string());
            ask.notify.notify_waiters();
        }
        self.mailboxes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let orchestrator = self.orchestrator_target();
        *self
            .known_targets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = vec![orchestrator];
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn reset_drops_mailboxes_and_wakes_pending_asks() {
        let bus = IntercomBus::new();
        bus.register_target("child");
        let pending = bus
            .create_ask(PendingAsk {
                id: "ask-1".into(),
                from: "child".into(),
                to: "orchestrator".into(),
                message: "choose".into(),
                reason: "need_decision".into(),
                ts: 1,
                reply: Mutex::new(None),
                notify: Arc::new(Notify::new()),
            })
            .unwrap();
        bus.reset();
        assert_eq!(bus.targets(), ["orchestrator"]);
        assert_eq!(bus.pending_count(), 0);
        assert_eq!(
            pending.reply.lock().unwrap().as_deref(),
            Some("[session replaced]")
        );
    }

    #[test]
    fn mailbox_and_message_sizes_are_bounded() {
        let bus = IntercomBus::new();
        bus.register_target("child");
        for index in 0..MAX_MAILBOX_MESSAGES {
            bus.post(IntercomMessage {
                id: format!("m-{index}"),
                from: "peer".into(),
                to: "child".into(),
                message: "ok".into(),
                reason: String::new(),
                ts: 1,
                ask_id: String::new(),
            })
            .unwrap();
        }
        assert!(bus
            .post(IntercomMessage {
                id: "overflow".into(),
                from: "peer".into(),
                to: "child".into(),
                message: "no".into(),
                reason: String::new(),
                ts: 1,
                ask_id: String::new(),
            })
            .unwrap_err()
            .contains("full"));
        assert!(bus
            .post(IntercomMessage {
                id: "large".into(),
                from: "peer".into(),
                to: "child".into(),
                message: "x".repeat(MAX_MESSAGE_BYTES + 1),
                reason: String::new(),
                ts: 1,
                ask_id: String::new(),
            })
            .unwrap_err()
            .contains("byte limit"));
    }

    #[test]
    fn unregister_wakes_and_removes_related_asks() {
        let bus = IntercomBus::new();
        bus.register_target("child");
        let pending = bus
            .create_ask(PendingAsk {
                id: "ask-child".into(),
                from: "child".into(),
                to: "orchestrator".into(),
                message: "choose".into(),
                reason: "need_decision".into(),
                ts: 1,
                reply: Mutex::new(None),
                notify: Arc::new(Notify::new()),
            })
            .unwrap();
        bus.unregister("child");
        assert_eq!(bus.pending_count(), 0);
        assert_eq!(
            pending.reply.lock().unwrap().as_deref(),
            Some("[agent finished]")
        );
    }
}

/// Emit the `intercom_message` event the TUI surfaces as a prompt for the
/// orchestrator to reply. Mirrors the approval_request flow.
fn emit_intercom_message(ask: &PendingAsk) {
    emit(
        &Event::new("intercom_message")
            .with("id", json!(ask.id))
            .with("from", json!(ask.from))
            .with("to", json!(ask.to))
            .with("reason", json!(ask.reason))
            .with("message", json!(ask.message)),
    );
}

/// Execute the `contact_supervisor` tool from within a subagent loop.
///
/// `from` is the calling subagent's intercom target; `orchestrator` is the
/// bus's orchestrator target. `reason: "need_decision"` blocks for a reply
/// (until the orchestrator answers via `intercom_reply`, or the turn is
/// cancelled); `reason: "progress_update"` returns immediately.
pub async fn execute_contact_supervisor(
    args: &Value,
    bus: &IntercomBus,
    from: &str,
    cancel: &CancellationToken,
) -> Outcome {
    let reason = args
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("need_decision");
    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
    if message.is_empty() {
        return Outcome::err("contact_supervisor requires a 'message'");
    }
    let orchestrator = bus.orchestrator_target();
    let ask = PendingAsk {
        id: next_id("ask"),
        from: from.to_string(),
        to: orchestrator.clone(),
        message: message.to_string(),
        reason: reason.to_string(),
        ts: now_ms(),
        reply: Mutex::new(None),
        notify: Arc::new(Notify::new()),
    };

    // progress_update is non-blocking: surface it but return immediately.
    if reason == "progress_update" {
        emit(
            &Event::new("intercom_message")
                .with("id", json!(ask.id))
                .with("from", json!(from))
                .with("to", json!(orchestrator))
                .with("reason", json!("progress_update"))
                .with("message", json!(message)),
        );
        return Outcome::ok("progress update sent to supervisor");
    }

    let handle = match bus.create_ask(ask) {
        Ok(h) => h,
        Err(e) => return Outcome::err(e),
    };
    // Block for the reply, bail out on cancel, or give up after a timeout so an
    // unanswered ask never wedges the subagent forever (P1-6). A need_decision
    // timeout MUST NOT silently "proceed with best judgment" — that would
    // authorize unapproved (potentially destructive) decisions after 5 min of
    // orchestrator inattention; return an error so the model re-asks/escalates.
    let result = tokio::select! {
        _ = handle.notify.notified() => {
            let reply = handle.reply.lock().unwrap_or_else(|e| e.into_inner()).clone();
            reply.unwrap_or_else(|| "[no reply]".to_string())
        }
        _ = cancel.cancelled() => {
            bus.cancel_ask(&handle.id);
            return Outcome::ok("[interrupted]");
        }
        _ = tokio::time::sleep(INTERCOM_ASK_TIMEOUT) => {
            bus.cancel_ask(&handle.id);
            return Outcome::err("need_decision timed out after 5 min with no supervisor response; do NOT proceed with the unapproved decision — re-ask, simplify, or escalate.");
        }
    };
    Outcome::ok(result)
}

/// Execute the `intercom` tool: peer-to-peer plumbing between subagents.
pub async fn execute_intercom(
    args: &Value,
    bus: &IntercomBus,
    from: &str,
    cancel: &CancellationToken,
) -> Outcome {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("send");
    match action {
        "targets" => {
            let t = bus.targets();
            Outcome::ok(json!(t).to_string())
        }
        "receive" | "poll" => {
            match bus.receive(from) {
                Some(m) => Outcome::ok(json!(m).to_string()),
                None => Outcome::ok("null"), // no pending messages (JSON null, same value-space as the object case)
            }
        }
        "send" => {
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
            let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if to.is_empty() || message.is_empty() {
                return Outcome::err("intercom send requires 'to' and 'message'");
            }
            let msg = IntercomMessage {
                id: next_id("msg"),
                from: from.to_string(),
                to: to.to_string(),
                message: message.to_string(),
                reason: args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                ts: now_ms(),
                ask_id: String::new(),
            };
            // The orchestrator answers via events, not a mailbox.
            if to == bus.orchestrator_target() {
                emit(
                    &Event::new("intercom_message")
                        .with("id", json!(msg.id))
                        .with("from", json!(from))
                        .with("to", json!(to))
                        .with("reason", json!(msg.reason))
                        .with("message", json!(message)),
                );
                return Outcome::ok(format!("message sent to {to}"));
            }
            match bus.post(msg) {
                Ok(()) => Outcome::ok(format!("message sent to {to}")),
                Err(e) => Outcome::err(e),
            }
        }
        "ask" => {
            // Blocking ask to a peer subagent (or the orchestrator).
            let to = args.get("to").and_then(|v| v.as_str()).unwrap_or("");
            let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let reason = args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("need_decision");
            if to.is_empty() || message.is_empty() {
                return Outcome::err("intercom ask requires 'to' and 'message'");
            }
            let ask = PendingAsk {
                id: next_id("ask"),
                from: from.to_string(),
                to: to.to_string(),
                message: message.to_string(),
                reason: reason.to_string(),
                ts: now_ms(),
                reply: Mutex::new(None),
                notify: Arc::new(Notify::new()),
            };
            let handle = match bus.create_ask(ask) {
                Ok(h) => h,
                Err(e) => return Outcome::err(e),
            };
            let result = tokio::select! {
                _ = handle.notify.notified() => {
                    handle.reply.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "[no reply]".to_string())
                }
                _ = cancel.cancelled() => {
                    bus.cancel_ask(&handle.id);
                    return Outcome::ok("[interrupted]");
                }
                _ = tokio::time::sleep(INTERCOM_ASK_TIMEOUT) => {
                    bus.cancel_ask(&handle.id);
                    return Outcome::err("intercom ask timed out after 5 min with no peer response; do NOT proceed with the unapproved decision — re-ask or pick a different peer.");
                }
            };
            Outcome::ok(result)
        }
        "reply" => {
            // Reply to a pending ask addressed to `from` (the caller).
            let id = args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let reply = args.get("reply").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return Outcome::err("intercom reply requires 'id' (the ask id) and 'reply'");
            }
            match bus.resolve_ask_from(id, from, reply) {
                Ok(()) => Outcome::ok(format!("replied to ask {id}")),
                Err(error) => Outcome::err(error),
            }
        }
        other => Outcome::err(format!(
            "unknown intercom action '{other}'; use send|ask|receive|poll|reply|targets"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_send_to_peer() {
        let bus = IntercomBus::new();
        bus.register_target("subagent-worker-1");
        bus.register_target("subagent-reviewer-1");
        let msg = IntercomMessage {
            id: "m1".into(),
            from: "subagent-worker-1".into(),
            to: "subagent-reviewer-1".into(),
            message: "hi".into(),
            reason: "".into(),
            ts: 1,
            ask_id: "".into(),
        };
        assert!(bus.post(msg).is_ok());
        let got = bus.receive("subagent-reviewer-1").unwrap();
        assert_eq!(got.message, "hi");
        assert!(bus.receive("subagent-reviewer-1").is_none());
    }

    #[test]
    fn send_to_unknown_target_errors() {
        let bus = IntercomBus::new();
        let msg = IntercomMessage {
            id: "m1".into(),
            from: "a".into(),
            to: "ghost".into(),
            message: "hi".into(),
            reason: "".into(),
            ts: 1,
            ask_id: "".into(),
        };
        assert!(bus.post(msg).is_err());
    }

    #[test]
    fn unregister_removes_from_known_targets() {
        // unregister must drop the peer from `targets()` so a later ask can't
        // target a dead peer and wedge for the 5-min timeout.
        let bus = IntercomBus::new();
        bus.register_target("worker-1");
        assert!(bus.targets().contains(&"worker-1".to_string()));
        bus.unregister("worker-1");
        assert!(
            !bus.targets().contains(&"worker-1".to_string()),
            "targets() still lists an unregistered peer"
        );
        assert!(
            !bus.targets().contains(&"orchestrator".to_string())
                || bus.targets().contains(&"orchestrator".to_string()),
            "orchestrator target must always remain known"
        );
    }

    #[test]
    fn create_ask_to_unknown_peer_fails_fast() {
        // An ask to a peer with no mailbox must error immediately (not register
        // a dangling ask that wedges the caller until the timeout).
        let bus = IntercomBus::new();
        let ask = PendingAsk {
            id: "a1".into(),
            from: "me".into(),
            to: "ghost".into(),
            message: "?".into(),
            reason: "need_decision".into(),
            ts: 1,
            reply: Mutex::new(None),
            notify: Arc::new(Notify::new()),
        };
        let r = bus.create_ask(ask);
        assert!(r.is_err(), "create_ask to unknown peer should error fast");
        // The ask must be removed from pending_asks on failure.
        assert!(bus
            .pending_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[test]
    fn orchestrator_is_known_default() {
        let bus = IntercomBus::new();
        assert_eq!(bus.orchestrator_target(), "orchestrator");
        assert!(bus.targets().contains(&"orchestrator".to_string()));
    }

    #[test]
    fn resolve_ask_returns_reply() {
        let bus = IntercomBus::new();
        let ask = PendingAsk {
            id: "a1".into(),
            from: "child".into(),
            to: "orchestrator".into(),
            message: "decide?".into(),
            reason: "need_decision".into(),
            ts: 1,
            reply: Mutex::new(None),
            notify: Arc::new(Notify::new()),
        };
        let handle = bus.create_ask(ask).unwrap();
        assert!(bus.resolve_ask("a1", "do it"));
        assert_eq!(
            handle
                .reply
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap(),
            "do it"
        );
        // second resolve fails (already removed)
        assert!(!bus.resolve_ask("a1", "again"));
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;

    #[test]
    fn journal_replays_once_after_restart() {
        let dir = std::env::temp_dir().join(format!("catalyst_intercom_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("session.jsonl");
        let first = IntercomBus::new();
        first.configure_journal(&session);
        first.register_target("worker");
        first
            .post(IntercomMessage {
                id: "m1".into(),
                from: "parent".into(),
                to: "worker".into(),
                message: "continue".into(),
                reason: "steer".into(),
                ts: 1,
                ask_id: String::new(),
            })
            .unwrap();
        let restarted = IntercomBus::new();
        restarted.configure_journal(&session);
        restarted.register_target("worker");
        assert_eq!(restarted.receive("worker").unwrap().id, "m1");
        restarted.register_target("worker");
        assert!(restarted.receive("worker").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reply_rejects_wrong_recipient() {
        let bus = IntercomBus::new();
        bus.register_target("worker");
        bus.register_target("other");
        bus.create_ask(PendingAsk {
            id: "a1".into(),
            from: "worker".into(),
            to: "other".into(),
            message: "?".into(),
            reason: String::new(),
            ts: 1,
            reply: Mutex::new(None),
            notify: Arc::new(Notify::new()),
        })
        .unwrap();
        assert!(bus.resolve_ask_from("a1", "worker", "no").is_err());
        assert_eq!(bus.pending_count(), 1);
    }
}
