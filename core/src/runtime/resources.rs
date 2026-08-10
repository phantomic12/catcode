use super::{RunId, SessionId};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio_util::sync::CancellationToken;

static RESOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Runtime-owned work that must not outlive its session or foreground run.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Task,
    Subprocess,
    Process,
    Approval,
    Ask,
    Sudo,
    Subagent,
    Goal,
    Browser,
    Intercom,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResourceSnapshot {
    pub id: u64,
    pub kind: ResourceKind,
    pub label: String,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub cancelled: bool,
}

#[derive(Debug)]
struct ResourceRecord {
    kind: ResourceKind,
    label: String,
    session_id: SessionId,
    run_id: Option<RunId>,
    cancellation: CancellationToken,
}

#[derive(Debug, Default)]
pub(crate) struct ResourceRegistry {
    records: Mutex<HashMap<u64, ResourceRecord>>,
}

impl ResourceRegistry {
    pub(crate) fn register(
        self: &Arc<Self>,
        kind: ResourceKind,
        label: impl Into<String>,
        session_id: SessionId,
        run_id: Option<RunId>,
        cancellation: CancellationToken,
    ) -> ResourceLease {
        let id = RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            id,
            ResourceRecord {
                kind,
                label: label.into(),
                session_id,
                run_id,
                cancellation: cancellation.clone(),
            },
        );
        ResourceLease {
            id,
            cancellation,
            registry: Arc::downgrade(self),
        }
    }

    pub(crate) fn cancel_run(&self, session_id: &SessionId, run_id: &RunId) {
        for record in self.lock().values() {
            if &record.session_id == session_id && record.run_id.as_ref() == Some(run_id) {
                record.cancellation.cancel();
            }
        }
    }

    pub(crate) fn cancel_session(&self, session_id: &SessionId) {
        for record in self.lock().values() {
            if &record.session_id == session_id {
                record.cancellation.cancel();
            }
        }
    }

    pub(crate) fn snapshots(&self) -> Vec<ResourceSnapshot> {
        let mut snapshots: Vec<_> = self
            .lock()
            .iter()
            .map(|(id, record)| ResourceSnapshot {
                id: *id,
                kind: record.kind,
                label: record.label.clone(),
                session_id: record.session_id.clone(),
                run_id: record.run_id.clone(),
                cancelled: record.cancellation.is_cancelled(),
            })
            .collect();
        snapshots.sort_by_key(|resource| resource.id);
        snapshots
    }

    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, ResourceRecord>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// RAII proof that a task or waiter is registered with the runtime. Dropping
/// the lease unregisters it; lifecycle cancellation can independently cancel
/// the token while the owner is still unwinding.
#[derive(Debug)]
pub struct ResourceLease {
    id: u64,
    cancellation: CancellationToken,
    registry: Weak<ResourceRegistry>,
}

impl ResourceLease {
    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.remove(self.id);
        }
    }
}
