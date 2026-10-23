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
    turn_starts: HashMap<ThreadId, usize>,
    manual_holds: HashSet<ThreadId>,
    retiring: HashSet<ThreadId>,
    retirement_checkpoints: HashSet<ThreadId>,
}

#[derive(Clone, Copy)]
enum ExecutionPermitKind {
    Operation,
    TurnStart,
    Recovery,
}

impl State {
    fn retirement_blocker(&self, root: ThreadId) -> Option<&'static str> {
        let entry = self.roots.get(&root)?;
        if entry.mode != ExecutionMode::Running || entry.reconciling {
            return Some("Waiting for the operator hold or reconciliation to clear.");
        }
        if self
            .manual_holds
            .iter()
            .any(|thread| self.root(*thread) == Some(root))
        {
            return Some("Waiting for an operator manual hold to clear.");
        }
        if entry.in_flight != 0 || self.turn_starts.get(&root).copied().unwrap_or_default() != 0 {
            return Some("Waiting for admitted operations to finish.");
        }
        None
    }

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
        state.turn_starts.remove(&root);
        state.retiring.remove(&root);
        state.retirement_checkpoints.remove(&root);
        self.changed.send_replace(());
    }

    /// Retirement fences only new turns; tools in admitted turns can finish.
    pub fn begin_retirement(&self, root: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retiring.insert(root) {
            self.changed.send_replace(());
        }
    }

    pub fn is_retiring(&self, thread_id: ThreadId) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .root(thread_id)
            .is_some_and(|root| state.retiring.contains(&root))
    }

    pub fn retirement_blocker(&self, root: ThreadId) -> Option<&'static str> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retirement_blocker(root)
    }

    /// Reserve the final idle check against new recovery starts. The caller
    /// keeps this short: history checkpoints precede this reservation.
    pub fn try_checkpoint_retirement(&self, root: ThreadId) -> Option<RetirementCheckpoint> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.retiring.contains(&root)
            || state.retirement_blocker(root).is_some()
            || !state.retirement_checkpoints.insert(root)
        {
            return None;
        }
        self.changed.send_replace(());
        Some(RetirementCheckpoint {
            control: self.clone(),
            root,
        })
    }

    /// The final boundary check shares the same lock as hold and turn admission.
    pub fn complete_retirement(&self, root: ThreadId) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.retiring.contains(&root) || state.retirement_blocker(root).is_some() {
            return false;
        }
        state.roots.remove(&root);
        state.turn_starts.remove(&root);
        state.retiring.remove(&root);
        state.retirement_checkpoints.remove(&root);
        self.changed.send_replace(());
        true
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
        self.try_enter_kind(thread_id, ExecutionPermitKind::Operation)
    }

    /// Count admitted startup through task registration separately from external calls.
    /// Stop must settle these starts before its final interruption scan; retained tool
    /// results can outlive that scan and remain ordinary execution permits.
    pub fn try_start_turn(&self, thread_id: ThreadId) -> Option<ExecutionPermit> {
        self.try_enter_kind(thread_id, ExecutionPermitKind::TurnStart)
    }

    /// Resume already-admitted work after an explicit operator recovery action.
    /// Retirement blocks new work, while existing holds still block recovery.
    pub fn try_resume_turn(&self, thread_id: ThreadId) -> Option<ExecutionPermit> {
        self.try_enter_kind(thread_id, ExecutionPermitKind::Recovery)
    }

    /// Number of admitted startups still able to register a task in this family.
    pub fn pending_turn_starts(&self, thread_id: ThreadId) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .root(thread_id)
            .and_then(|root| state.turn_starts.get(&root).copied())
            .unwrap_or_default()
    }

    fn try_enter_kind(
        &self,
        thread_id: ThreadId,
        kind: ExecutionPermitKind,
    ) -> Option<ExecutionPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = state.root(thread_id);
        if let Some(root) = root {
            if (matches!(kind, ExecutionPermitKind::TurnStart) && state.retiring.contains(&root))
                || (matches!(
                    kind,
                    ExecutionPermitKind::TurnStart | ExecutionPermitKind::Recovery
                ) && state.retirement_checkpoints.contains(&root))
            {
                return None;
            }
            if state.manually_held(thread_id) {
                return None;
            }
            let entry = state.roots.get_mut(&root)?;
            if entry.mode != ExecutionMode::Running || entry.reconciling {
                return None;
            }
            entry.in_flight += 1;
            if matches!(
                kind,
                ExecutionPermitKind::TurnStart | ExecutionPermitKind::Recovery
            ) {
                *state.turn_starts.entry(root).or_default() += 1;
            }
            self.changed.send_replace(());
        }
        let revision = root.and_then(|root| state.roots.get(&root).map(|entry| entry.revision));
        Some(ExecutionPermit {
            control: self.clone(),
            root,
            revision,
            kind,
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

/// Holds the final runtime idle check against concurrent recovery admission.
/// Dropping it preserves retirement and wakes pending recovery owners.
pub struct RetirementCheckpoint {
    control: ExecutionControl,
    root: ThreadId,
}

impl Drop for RetirementCheckpoint {
    fn drop(&mut self) {
        let mut state = self
            .control
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.retirement_checkpoints.remove(&self.root) {
            self.control.changed.send_replace(());
        }
    }
}

/// Counts an admitted operation until it finishes or is explicitly cancelled.
pub struct ExecutionPermit {
    control: ExecutionControl,
    root: Option<ThreadId>,
    revision: Option<u64>,
    kind: ExecutionPermitKind,
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
        if matches!(
            self.kind,
            ExecutionPermitKind::TurnStart | ExecutionPermitKind::Recovery
        ) && let Some(starting) = state.turn_starts.get_mut(&root)
        {
            *starting = starting.saturating_sub(1);
            if *starting == 0 {
                state.turn_starts.remove(&root);
            }
        }
        self.control.changed.send_replace(());
    }
}
