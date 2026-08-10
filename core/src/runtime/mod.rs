//! Runtime lifecycle ownership for sessions and agent runs.
//!
//! This module deliberately contains no protocol or agent-loop logic.  It is
//! the authority that creates identities, owns cancellation tokens, and decides
//! whether an asynchronous result still belongs to the active session/run.

mod coordinator;
mod event_sink;
mod lifecycle;
mod process_supervisor;
mod resources;
mod run;

pub use coordinator::RuntimeCoordinator;
pub use event_sink::{current_run_id, current_session_id, scope_run, scope_session, EventSink};
pub use lifecycle::{CancellationReason, CancelledRun};
pub use process_supervisor::{
    NamedProcessSupervisor, ProcessError, ProcessLogs, ProcessSpec, ProcessState, ProcessStatus,
    Readiness,
};
pub use resources::{ResourceKind, ResourceLease, ResourceSnapshot};
pub use run::{RunContext, RunId, SessionContext, SessionId};
