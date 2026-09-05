use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use tokio::sync::watch;

const MAX_CONCURRENT_ADMISSIONS: usize = 64;
const MAX_PENDING_CELL_IDS: usize = 64;
const MAX_PENDING_CALLBACKS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CellAdmissionResolution {
    Pending,
    Admitted,
    Rejected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CellAdmissionOutcome {
    Admitted,
    CloseRequested,
    Rejected,
}

#[derive(Clone, Default)]
pub(super) struct CellAdmissionRegistry {
    inner: Arc<Mutex<CellAdmissionState>>,
}

#[derive(Default)]
struct CellAdmissionState {
    closed: bool,
    next_attempt_id: u64,
    active_attempts: HashSet<u64>,
    pending_cells: HashMap<CellId, PendingCellAdmission>,
    pending_callback_count: usize,
}

struct PendingCellAdmission {
    callbacks: Vec<PendingCallbackAdmission>,
    close_candidate_attempts: HashSet<u64>,
}

struct PendingCallbackAdmission {
    candidate_attempts: HashSet<u64>,
    resolution: watch::Sender<CellAdmissionResolution>,
}

type PendingCallbackResolutions = Vec<(
    watch::Sender<CellAdmissionResolution>,
    CellAdmissionResolution,
)>;

struct ResolvedCellAdmissionAttempt {
    resolutions: PendingCallbackResolutions,
    close_requested: bool,
}

pub(super) struct CellAdmissionAttempt {
    registry: CellAdmissionRegistry,
    attempt_id: u64,
    resolved: bool,
}

impl CellAdmissionRegistry {
    pub(super) fn begin(&self) -> Result<CellAdmissionAttempt, String> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err("code mode session is shutting down".to_string());
        }
        if state.active_attempts.len() >= MAX_CONCURRENT_ADMISSIONS {
            return Err("too many code-mode cell admissions are in progress".to_string());
        }
        let attempt_id = state.next_attempt_id;
        state.next_attempt_id = state
            .next_attempt_id
            .checked_add(1)
            .ok_or_else(|| "code-mode cell admission counter exhausted".to_string())?;
        state.active_attempts.insert(attempt_id);
        Ok(CellAdmissionAttempt {
            registry: self.clone(),
            attempt_id,
            resolved: false,
        })
    }

    pub(super) fn subscribe_callback(
        &self,
        cell_id: &CellId,
    ) -> Result<Option<watch::Receiver<CellAdmissionResolution>>, String> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_attempts.is_empty() {
            return Ok(None);
        }
        if state.pending_callback_count >= MAX_PENDING_CALLBACKS {
            return Err("too many code-mode callbacks are awaiting cell admission".to_string());
        }
        if !state.pending_cells.contains_key(cell_id)
            && state.pending_cells.len() >= MAX_PENDING_CELL_IDS
        {
            return Err("too many code-mode cell IDs are awaiting admission".to_string());
        }

        let active_attempts = state.active_attempts.clone();
        let pending = state
            .pending_cells
            .entry(cell_id.clone())
            .or_insert_with(|| PendingCellAdmission {
                callbacks: Vec::new(),
                close_candidate_attempts: HashSet::new(),
            });
        let (resolution, receiver) = watch::channel(CellAdmissionResolution::Pending);
        pending.callbacks.push(PendingCallbackAdmission {
            candidate_attempts: active_attempts,
            resolution,
        });
        state.pending_callback_count = state.pending_callback_count.saturating_add(1);
        Ok(Some(receiver))
    }

    pub(super) fn buffer_cell_closed(&self, cell_id: &CellId) -> Result<bool, String> {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_attempts.is_empty() {
            return Ok(false);
        }
        if !state.pending_cells.contains_key(cell_id)
            && state.pending_cells.len() >= MAX_PENDING_CELL_IDS
        {
            return Err("too many code-mode cell IDs are awaiting admission".to_string());
        }

        let active_attempts = state.active_attempts.clone();
        let pending = state
            .pending_cells
            .entry(cell_id.clone())
            .or_insert_with(|| PendingCellAdmission {
                callbacks: Vec::new(),
                close_candidate_attempts: HashSet::new(),
            });
        pending.close_candidate_attempts.extend(active_attempts);
        Ok(true)
    }

    pub(super) fn reject_all(&self) {
        let resolutions = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.active_attempts.clear();
            state.pending_callback_count = 0;
            state
                .pending_cells
                .drain()
                .flat_map(|(_, pending)| {
                    pending
                        .callbacks
                        .into_iter()
                        .map(|callback| callback.resolution)
                })
                .collect::<Vec<_>>()
        };
        for resolution in resolutions {
            resolution.send_replace(CellAdmissionResolution::Rejected);
        }
    }

    fn admit_attempt(
        &self,
        attempt_id: u64,
        cell_id: &CellId,
        on_admit: impl FnOnce() -> Result<(), String>,
    ) -> Result<CellAdmissionOutcome, String> {
        let (resolutions, outcome) = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed || !state.active_attempts.contains(&attempt_id) {
                return Ok(CellAdmissionOutcome::Rejected);
            }
            if let Err(error) = on_admit() {
                let Some(resolved) = resolve_attempt(&mut state, attempt_id, None) else {
                    unreachable!("active admission attempt should resolve");
                };
                drop(state);
                send_resolutions(resolved.resolutions);
                return Err(error);
            }
            let Some(resolved) = resolve_attempt(&mut state, attempt_id, Some(cell_id)) else {
                unreachable!("active admission attempt should resolve");
            };
            let outcome = if resolved.close_requested {
                CellAdmissionOutcome::CloseRequested
            } else {
                CellAdmissionOutcome::Admitted
            };
            (resolved.resolutions, outcome)
        };
        send_resolutions(resolutions);
        Ok(outcome)
    }

    fn reject_attempt(&self, attempt_id: u64) {
        let resolutions = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(resolved) = resolve_attempt(&mut state, attempt_id, None) else {
                return;
            };
            resolved.resolutions
        };
        send_resolutions(resolutions);
    }
}

fn resolve_attempt(
    state: &mut CellAdmissionState,
    attempt_id: u64,
    admitted_cell_id: Option<&CellId>,
) -> Option<ResolvedCellAdmissionAttempt> {
    if !state.active_attempts.remove(&attempt_id) {
        return None;
    }

    let mut resolutions = Vec::new();
    let mut resolved_cell_ids = Vec::new();
    let mut resolved_callback_count = 0usize;
    let mut close_requested = false;
    for (cell_id, pending) in &mut state.pending_cells {
        let cell_was_admitted = admitted_cell_id == Some(cell_id);
        let mut callback_index = 0;
        while callback_index < pending.callbacks.len() {
            let callback = &mut pending.callbacks[callback_index];
            let was_candidate = callback.candidate_attempts.remove(&attempt_id);
            let resolution = if was_candidate && cell_was_admitted {
                Some(CellAdmissionResolution::Admitted)
            } else if callback.candidate_attempts.is_empty() {
                Some(CellAdmissionResolution::Rejected)
            } else {
                None
            };
            let Some(resolution) = resolution else {
                callback_index += 1;
                continue;
            };
            let callback = pending.callbacks.swap_remove(callback_index);
            resolutions.push((callback.resolution, resolution));
            resolved_callback_count = resolved_callback_count.saturating_add(1);
        }

        if pending.close_candidate_attempts.remove(&attempt_id) && cell_was_admitted {
            close_requested = true;
        }
        if pending.callbacks.is_empty() && pending.close_candidate_attempts.is_empty() {
            resolved_cell_ids.push(cell_id.clone());
        }
    }
    for cell_id in resolved_cell_ids {
        state.pending_cells.remove(&cell_id);
    }
    state.pending_callback_count = state
        .pending_callback_count
        .saturating_sub(resolved_callback_count);
    Some(ResolvedCellAdmissionAttempt {
        resolutions,
        close_requested,
    })
}

fn send_resolutions(resolutions: PendingCallbackResolutions) {
    for (resolution, value) in resolutions {
        resolution.send_replace(value);
    }
}

impl CellAdmissionAttempt {
    pub(super) fn admit(
        mut self,
        cell_id: &CellId,
        on_admit: impl FnOnce() -> Result<(), String>,
    ) -> Result<CellAdmissionOutcome, String> {
        let outcome = self
            .registry
            .admit_attempt(self.attempt_id, cell_id, on_admit);
        self.resolved = true;
        outcome
    }
}

impl Drop for CellAdmissionAttempt {
    fn drop(&mut self) {
        if !self.resolved {
            self.registry.reject_attempt(self.attempt_id);
        }
    }
}
