//! Cooperative execution admission shared by a host and its thread runtimes.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_protocol::ThreadId;
use tokio::sync::watch;

/// A hold closes admission without cancelling operations already admitted.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ExecutionMode {
    #[default]
    Running,
    Paused,
    Stopped,
}

/// Snapshot of one enrolled root, including work admitted for its descendants.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionControlSnapshot {
    pub revision: u64,
    pub mode: ExecutionMode,
    pub reconciling: bool,
    pub in_flight: usize,
}

#[derive(Default)]
struct State {
    parents: HashMap<ThreadId, ThreadId>,
    roots: HashMap<ThreadId, ExecutionControlSnapshot>,
    manual_holds: HashSet<ThreadId>,
}

impl State {
    fn manually_held(&self, mut thread_id: ThreadId) -> bool {
        let mut visited = HashSet::new();
        while visited.insert(thread_id) {
            if self.manual_holds.contains(&thread_id) {
                return true;
            }
            let Some(parent) = self.parents.get(&thread_id) else {
                break;
            };
            thread_id = *parent;
        }
        false
    }

    fn root(&self, mut thread_id: ThreadId) -> Option<ThreadId> {
        let mut visited = HashSet::new();
        while visited.insert(thread_id) {
            if self.roots.contains_key(&thread_id) {
                return Some(thread_id);
            }
            thread_id = *self.parents.get(&thread_id)?;
        }
        None
    }
}

/// Host-owned admission state. Persistence and cancellation belong to the host.
///
/// All state transitions and permit acquisition share a lock. Closing admission
/// therefore cannot race with an operation being counted as newly admitted.
#[derive(Clone)]
pub struct ExecutionControl {
    state: Arc<Mutex<State>>,
    changed: watch::Sender<()>,
}

impl Default for ExecutionControl {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            changed: watch::channel(()).0,
        }
    }
}

impl ExecutionControl {
    /// A local interruption survives later coordinator resumes.
    pub fn hold_manually(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.manual_holds.insert(thread_id);
        self.changed.send_replace(());
    }

    pub fn is_manually_held(&self, thread_id: ThreadId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .manually_held(thread_id)
    }

    /// Explicit local user input can release its own hold, while a global hold
    /// and holds on ancestors remain authoritative.
    pub fn resume_for_user(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .root(thread_id)
            .and_then(|root| state.roots.get(&root))
            .is_some_and(|entry| entry.mode == ExecutionMode::Running && !entry.reconciling)
        {
            state.manual_holds.remove(&thread_id);
            self.changed.send_replace(());
        }
    }

    /// Restore durable state before loading sessions. A coordinator must confirm
    /// this revision or a newer one before coordinated execution can resume.
    pub fn restore(&self, root: ThreadId, revision: u64, mode: ExecutionMode) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.roots.insert(
            root,
            ExecutionControlSnapshot {
                revision,
                mode,
                reconciling: true,
                in_flight: 0,
            },
        );
        self.changed.send_replace(());
    }

    /// Record lineage before a child can execute, including unenrolled parents.
    pub fn inherit(&self, thread_id: ThreadId, parent_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if thread_id != parent_id {
            state.parents.insert(thread_id, parent_id);
        }
        self.changed.send_replace(());
    }

    /// Restore or update an enrolled root. A stale revision never changes its mode.
    /// Equal revisions are idempotent, even when their supplied modes differ.
    pub fn apply(
        &self,
        root: ThreadId,
        revision: u64,
        mode: ExecutionMode,
    ) -> ExecutionControlSnapshot {
        self.apply_reconciling(root, revision, mode, false)
    }

    /// Apply desired admission while retaining an unresolved local Stop fence.
    pub fn apply_reconciling(
        &self,
        root: ThreadId,
        revision: u64,
        mode: ExecutionMode,
        reconciling: bool,
    ) -> ExecutionControlSnapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.roots.entry(root).or_insert(ExecutionControlSnapshot {
            revision,
            mode,
            reconciling: false,
            in_flight: 0,
        });
        if revision > entry.revision {
            entry.revision = revision;
            entry.mode = mode;
        }
        if revision == entry.revision && mode == entry.mode {
            entry.reconciling = reconciling;
        }
        let snapshot = *entry;
        self.changed.send_replace(());
        snapshot
    }

    /// Remove the family boundary after its host has verified local Stop is
    /// settled. Keep lineage and manual holds for later enrollment.
    pub fn unenroll(&self, root: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.roots.remove(&root);
        self.changed.send_replace(());
    }

    /// Return the controlling root, if this thread belongs to an enrolled family.
    pub fn root(&self, thread_id: ThreadId) -> Option<ThreadId> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .root(thread_id)
    }

    pub fn snapshot(&self, thread_id: ThreadId) -> Option<ExecutionControlSnapshot> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .root(thread_id)
            .and_then(|root| state.roots.get(&root).copied())
    }

    /// Return immediately when held. Session input loops must never await a hold.
    pub fn try_enter(&self, thread_id: ThreadId) -> Option<ExecutionPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = state.root(thread_id);
        if let Some(root) = root {
            if state.manually_held(thread_id) {
                return None;
            }
            let entry = state.roots.get_mut(&root)?;
            if entry.mode != ExecutionMode::Running || entry.reconciling {
                return None;
            }
            entry.in_flight += 1;
            self.changed.send_replace(());
        }
        let revision = root.and_then(|root| state.roots.get(&root).map(|entry| entry.revision));
        Some(ExecutionPermit {
            control: self.clone(),
            root,
            revision,
        })
    }

    /// Wait without polling. Callers retain their existing cancellation handling.
    pub async fn enter(&self, thread_id: ThreadId) -> ExecutionPermit {
        let mut changed = self.changed.subscribe();
        loop {
            if let Some(permit) = self.try_enter(thread_id) {
                return permit;
            }
            // The sender is retained by self, so this subscription cannot close.
            let _ = changed.changed().await;
        }
    }

    /// Observe admission and completion changes without running an agent turn.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }
}

/// Counts an admitted operation until it finishes or is explicitly cancelled.
pub struct ExecutionPermit {
    control: ExecutionControl,
    root: Option<ThreadId>,
    revision: Option<u64>,
}

impl ExecutionPermit {
    /// Revision captured atomically when this work passed admission.
    pub fn revision(&self) -> Option<u64> {
        self.revision
    }
}

/// Per-call admission provenance retained across tool dispatch and code-mode waits.
#[derive(Default)]
pub struct ExecutionAdmissions(Mutex<HashMap<String, u64>>);

impl ExecutionAdmissions {
    pub fn record(&self, call_id: String, revision: u64) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(call_id)
            .or_insert(revision);
    }

    pub fn revision(&self, call_id: &str) -> Option<u64> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(call_id)
            .copied()
    }
}

/// Calls with a durable host owner. Interruption may discard pending calls;
/// once dispatch begins, the original handle must publish its eventual result.
#[derive(Default)]
pub struct RetainedToolResults(Mutex<HashMap<String, RetainedCallPhase>>);

enum RetainedCallPhase {
    Pending,
    Dispatched,
    Cancelled,
}

impl RetainedToolResults {
    pub fn retain(&self, call_id: String) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(call_id)
            .or_insert(RetainedCallPhase::Pending);
    }

    /// Linearize external dispatch against interruption. A cancelled pending
    /// intent can never begin an effect after admission is reopened.
    pub fn begin_dispatch(&self, call_id: &str) -> bool {
        let mut calls = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match calls.get_mut(call_id) {
            Some(RetainedCallPhase::Cancelled) => false,
            Some(phase @ RetainedCallPhase::Pending) => {
                *phase = RetainedCallPhase::Dispatched;
                true
            }
            Some(RetainedCallPhase::Dispatched) | None => true,
        }
    }

    /// Preserve only an original in-flight call. Claiming cancellation of a
    /// pending intent atomically prevents its later dispatch callback.
    pub fn retain_after_cancellation(&self, call_id: &str) -> bool {
        let mut calls = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match calls.get_mut(call_id) {
            Some(RetainedCallPhase::Dispatched) => true,
            Some(phase @ RetainedCallPhase::Pending) => {
                *phase = RetainedCallPhase::Cancelled;
                false
            }
            Some(RetainedCallPhase::Cancelled) | None => false,
        }
    }
}

impl Drop for ExecutionPermit {
    fn drop(&mut self) {
        let Some(root) = self.root else { return };
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.roots.get_mut(&root) {
            entry.in_flight = entry.in_flight.saturating_sub(1);
        }
        self.control.changed.send_replace(());
    }
}
