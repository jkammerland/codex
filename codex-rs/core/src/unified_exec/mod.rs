//! Unified Exec: interactive process execution orchestrated with approvals + sandboxing.
//!
//! Responsibilities
//! - Manages interactive processes (create, reuse, buffer output with caps).
//! - Supports completion-only calls that terminate on timeout or cancellation.
//! - Uses the shared ToolOrchestrator to handle approval, sandbox selection, and
//!   retry semantics in a single, descriptive flow.
//! - Spawns the PTY from a sandbox-transformed `ExecRequest`; on sandbox denial,
//!   retries without sandbox when policy allows (no re‑prompt thanks to caching).
//! - Uses the shared `is_likely_sandbox_denied` heuristic to keep denial messages
//!   consistent with other exec paths.
//!
//! Flow at a glance (open process)
//! 1) Build a small request `{ command, cwd }`.
//! 2) Orchestrator: approval (bypass/cache/prompt) → select sandbox → run.
//! 3) Runtime: transform `SandboxTransformRequest` -> `ExecRequest` -> spawn PTY.
//! 4) If denial, orchestrator retries with `SandboxType::None`.
//! 5) Process handle is returned with streaming output + metadata.
//!
//! This keeps policy logic and user interaction centralized while the PTY/process
//! concerns remain isolated here. The implementation is split between:
//! - `process.rs`: PTY process lifecycle + output buffering.
//! - `process_state.rs`: shared exit/failure state for local and remote processes.
//! - `process_manager.rs`: orchestration (approvals, sandboxing, reuse) and request handling.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;

use codex_network_proxy::NetworkProxy;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_path_uri::PathUri;
use rand::Rng;
use rand::rng;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::sandboxing::SandboxPermissions;
use crate::session::InputQueueActivity;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::ShellType;
use crate::tools::network_approval::DeferredNetworkApproval;
use codex_core_plugins::PluginMetricsSidecar;

mod async_watcher;
mod errors;
mod head_tail_buffer;
mod oneshot;
mod pending_launch;
mod pending_launch_supervisor;
mod process;
mod process_manager;
mod process_state;
mod shell_snapshot;
mod stdin_approval;

pub(crate) fn set_deterministic_process_ids_for_tests(enabled: bool) {
    process_manager::set_deterministic_process_ids_for_tests(enabled);
}

pub(crate) use errors::UnifiedExecError;
use pending_launch::PendingLaunch;
pub(crate) use process::NoopSpawnLifecycle;
#[cfg(unix)]
pub(crate) use process::SpawnLifecycle;
pub(crate) use process::SpawnLifecycleHandle;
pub(crate) use process::UnifiedExecProcess;
pub(crate) use stdin_approval::TerminalPermissions;
pub(crate) use stdin_approval::TerminalSandboxSource;

pub(crate) const MIN_YIELD_TIME_MS: u64 = 250;
pub(crate) const WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS: u64 = 10_000;
// Minimum yield time for an empty `write_stdin`.
pub(crate) const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub(crate) const MAX_YIELD_TIME_MS: u64 = 30_000;
pub(crate) const DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS: u64 = 300_000;
pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_BYTES: usize = 1024 * 1024; // 1 MiB
pub(crate) const UNIFIED_EXEC_OUTPUT_MAX_TOKENS: usize = UNIFIED_EXEC_OUTPUT_MAX_BYTES / 4;
pub(crate) const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;

pub(crate) struct UnifiedExecContext {
    pub session: Arc<Session>,
    pub step_context: Arc<StepContext>,
    pub cancellation_token: CancellationToken,
    pub call_id: String,
}

impl UnifiedExecContext {
    pub fn new(
        session: Arc<Session>,
        step_context: Arc<StepContext>,
        cancellation_token: CancellationToken,
        call_id: String,
    ) -> Self {
        Self {
            session,
            step_context,
            cancellation_token,
            call_id,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ExecCommandRequest {
    pub command: Vec<String>,
    pub shell_type: ShellType,
    pub hook_command: String,
    pub process_id_lease: ProcessIdLease,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub cwd: PathUri,
    pub sandbox_cwd: PathUri,
    pub turn_environment: TurnEnvironment,
    pub shell_mode: UnifiedExecShellMode,
    pub network: Option<NetworkProxy>,
    pub tty: bool,
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub additional_permissions_preapproved: bool,
    pub justification: Option<String>,
    pub prefix_rule: Option<Vec<String>>,
}

#[derive(Debug)]
pub(crate) struct WriteStdinRequest<'a> {
    pub process_id: i32,
    pub input: &'a str,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
    pub interaction_event: Option<UnifiedExecInteractionEvent<'a>>,
}

#[derive(Debug)]
pub(crate) struct WaitProcessRequest<'a> {
    pub process_id: i32,
    pub max_output_tokens: Option<usize>,
    pub truncation_policy: TruncationPolicy,
    pub interaction_event: Option<UnifiedExecInteractionEvent<'a>>,
    pub activity_rx: &'a mut watch::Receiver<InputQueueActivity>,
    pub pending_activity: Option<InputQueueActivity>,
}

pub(crate) struct UnifiedExecInteractionEvent<'a> {
    pub session: &'a Arc<Session>,
    pub turn: &'a Arc<TurnContext>,
}

impl std::fmt::Debug for UnifiedExecInteractionEvent<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UnifiedExecInteractionEvent")
    }
}

pub(crate) struct ProcessStore {
    processes: HashMap<i32, ProcessEntry>,
    reserved_process_ids: Arc<ProcessIdReservations>,
    next_deterministic_process_id: i32,
}

impl Default for ProcessStore {
    fn default() -> Self {
        Self {
            processes: HashMap::new(),
            reserved_process_ids: Arc::new(ProcessIdReservations::default()),
            next_deterministic_process_id: 1000,
        }
    }
}

#[derive(Debug, Default)]
struct ProcessIdReservations {
    // This registry is independently synchronized so an allocation-origin
    // guard can release its reservation synchronously when its future is
    // dropped. Process storage remains on the async manager lock.
    leases: std::sync::Mutex<HashMap<i32, ProcessIdLease>>,
}

impl ProcessIdReservations {
    fn contains_key(&self, process_id: &i32) -> bool {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(process_id)
    }

    fn insert(&self, process_id: i32, lease: ProcessIdLease) -> Option<ProcessIdLease> {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(process_id, lease)
    }

    fn matches(&self, lease: &ProcessIdLease) -> bool {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&lease.process_id())
            .is_some_and(|current| current.same_allocation(lease))
    }

    fn release(&self, process_id: i32, reservation: &Arc<ProcessIdReservation>) {
        let mut leases = self
            .leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if leases
            .get(&process_id)
            .is_some_and(|current| Arc::ptr_eq(&current.reservation, reservation))
        {
            leases.remove(&process_id);
        }
    }

    fn clear(&self) {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    fn snapshot(&self) -> Vec<ProcessIdLease> {
        self.leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

impl ProcessStore {
    fn reservation_matches(&self, process_id_lease: &ProcessIdLease) -> bool {
        self.reserved_process_ids.matches(process_id_lease)
    }

    fn insert(&mut self, entry: ProcessEntry) -> Result<(), Box<ProcessEntry>> {
        let process_id = entry.process_id_lease.process_id();
        if entry.process_id != process_id
            || !self.reservation_matches(&entry.process_id_lease)
            || self.processes.contains_key(&process_id)
        {
            return Err(Box::new(entry));
        }
        self.processes.insert(process_id, entry);
        Ok(())
    }

    fn release_reservation(&mut self, process_id_lease: &ProcessIdLease) {
        self.reserved_process_ids
            .release(process_id_lease.process_id(), &process_id_lease.reservation);
    }

    fn remove(&mut self, process_id_lease: &ProcessIdLease) -> Option<ProcessEntry> {
        let process_id = process_id_lease.process_id();
        let entry_matches = self
            .processes
            .get(&process_id)
            .is_some_and(|entry| entry.process_id_lease.same_allocation(process_id_lease));
        let entry = if entry_matches {
            self.processes.remove(&process_id)
        } else {
            None
        };
        self.release_reservation(process_id_lease);
        entry
    }

    fn remove_if_process_matches(
        &mut self,
        process_id_lease: &ProcessIdLease,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> Option<ProcessEntry> {
        let process_id = process_id_lease.process_id();
        match self.processes.get(&process_id) {
            Some(entry)
                if entry.process_id_lease.same_allocation(process_id_lease)
                    && Arc::ptr_eq(&entry.process, expected_process) =>
            {
                // Keep the allocation reserved until its manager-owned
                // lifecycle watcher has finalized output, approval, metrics,
                // and the terminal event.
                self.processes.remove(&process_id)
            }
            Some(_) => None,
            None => None,
        }
    }
}

#[derive(Debug)]
struct ProcessIdReservation;

pub(crate) struct ProcessIdLease {
    process_id: i32,
    reservation: Arc<ProcessIdReservation>,
    pending_launch: Arc<PendingLaunch>,
    auto_release: Option<ProcessIdReservationGuard>,
}

struct ProcessIdReservationGuard {
    process_id: i32,
    reservation: Arc<ProcessIdReservation>,
    pending_launch: Arc<PendingLaunch>,
    reservations: Weak<ProcessIdReservations>,
    armed: bool,
}

impl ProcessIdLease {
    #[cfg(test)]
    fn new(process_id: i32) -> Self {
        Self {
            process_id,
            reservation: Arc::new(ProcessIdReservation),
            pending_launch: PendingLaunch::new(),
            auto_release: None,
        }
    }

    fn new_reserved(process_id: i32, reservations: &Arc<ProcessIdReservations>) -> Self {
        let reservation = Arc::new(ProcessIdReservation);
        let pending_launch = PendingLaunch::new();
        Self {
            process_id,
            reservation: Arc::clone(&reservation),
            pending_launch: Arc::clone(&pending_launch),
            auto_release: Some(ProcessIdReservationGuard {
                process_id,
                reservation,
                pending_launch,
                reservations: Arc::downgrade(reservations),
                armed: true,
            }),
        }
    }

    pub(crate) fn process_id(&self) -> i32 {
        self.process_id
    }

    fn same_allocation(&self, other: &Self) -> bool {
        self.process_id == other.process_id && Arc::ptr_eq(&self.reservation, &other.reservation)
    }

    fn pending_launch(&self) -> &Arc<PendingLaunch> {
        &self.pending_launch
    }

    fn disarm_auto_release(&mut self) {
        if let Some(mut auto_release) = self.auto_release.take() {
            auto_release.armed = false;
        }
    }
}

impl Clone for ProcessIdLease {
    fn clone(&self) -> Self {
        // Only the allocation-origin lease owns automatic release. Manager and
        // process-store clones retain ABA identity without extending that
        // cancellation responsibility.
        Self {
            process_id: self.process_id,
            reservation: Arc::clone(&self.reservation),
            pending_launch: Arc::clone(&self.pending_launch),
            auto_release: None,
        }
    }
}

impl Drop for ProcessIdReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.pending_launch.request_cleanup();
        if let Some(reservations) = self.reservations.upgrade() {
            reservations.release(self.process_id, &self.reservation);
        }
    }
}

impl std::fmt::Debug for ProcessIdLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProcessIdLease")
            .field("process_id", &self.process_id)
            .finish_non_exhaustive()
    }
}

pub(crate) struct UnifiedExecProcessManager {
    process_store: Mutex<ProcessStore>,
    launch_process_ownership: std::sync::Mutex<HashMap<i32, ProcessIdLease>>,
    max_write_stdin_yield_time_ms: u64,
}

impl UnifiedExecProcessManager {
    pub(crate) fn new(max_write_stdin_yield_time_ms: u64) -> Self {
        Self {
            process_store: Mutex::new(ProcessStore::default()),
            launch_process_ownership: std::sync::Mutex::new(HashMap::new()),
            max_write_stdin_yield_time_ms: max_write_stdin_yield_time_ms
                .max(MIN_EMPTY_YIELD_TIME_MS),
        }
    }
}

impl Default for UnifiedExecProcessManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BACKGROUND_TERMINAL_TIMEOUT_MS)
    }
}

struct LaunchProcessOwnershipGuard<'a> {
    ownership: &'a std::sync::Mutex<HashMap<i32, ProcessIdLease>>,
    process_id_lease: ProcessIdLease,
}

impl Drop for LaunchProcessOwnershipGuard<'_> {
    fn drop(&mut self) {
        let mut ownership = self
            .ownership
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ownership
            .get(&self.process_id_lease.process_id())
            .is_some_and(|lease| lease.same_allocation(&self.process_id_lease))
        {
            ownership.remove(&self.process_id_lease.process_id());
        }
    }
}

struct ProcessEntry {
    process: Arc<UnifiedExecProcess>,
    process_id_lease: ProcessIdLease,
    call_id: String,
    process_id: i32,
    cwd: PathUri,
    initial_exec_command_active: Arc<std::sync::atomic::AtomicBool>,
    hook_command: String,
    tty: bool,
    environment_id: String,
    permissions: TerminalPermissions,
    network_approval: Option<DeferredNetworkApproval>,
    session: Weak<Session>,
    last_used: tokio::time::Instant,
}

type SharedPluginMetricsSidecar = Arc<std::sync::Mutex<Option<PluginMetricsSidecar>>>;

fn take_plugin_metrics_sidecar(
    sidecar: &SharedPluginMetricsSidecar,
) -> Option<PluginMetricsSidecar> {
    sidecar
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

pub(crate) fn clamp_yield_time(yield_time_ms: u64) -> u64 {
    let yield_time_ms = if cfg!(windows) {
        yield_time_ms.max(WINDOWS_INITIAL_EXEC_YIELD_TIME_FLOOR_MS)
    } else {
        yield_time_ms
    };
    yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
}

pub(crate) fn resolve_max_tokens(max_tokens: Option<usize>) -> usize {
    max_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

pub(crate) fn format_output_omission_marker(omitted_bytes: usize) -> String {
    format!("... {omitted_bytes} bytes omitted ...")
}

pub(crate) fn generate_chunk_id() -> String {
    let mut rng = rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod process_tests;
#[cfg(test)]
#[cfg(unix)]
#[path = "mod_tests.rs"]
mod tests;
