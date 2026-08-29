use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

const PENDING: u8 = 0;
const HANDLER_RUNNING: u8 = 1;
const HANDLER_COMPLETED: u8 = 2;
const COOPERATIVE_CANCELLING: u8 = 3;
const TERMINAL_CLAIMED: u8 = 4;

/// Coordinates handler admission and terminal lifecycle ownership across the dispatcher task and
/// its cancellation watcher.
pub(crate) struct ToolCallLifecycle {
    state: AtomicU8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancellationDisposition {
    AbortDispatch,
    AwaitDispatch,
}

impl ToolCallLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            state: AtomicU8::new(PENDING),
        }
    }

    /// Atomically admits the handler only while cancellation has not claimed the call.
    pub(crate) fn try_start_handler(&self) -> bool {
        self.state
            .compare_exchange(
                PENDING,
                HANDLER_RUNNING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Publishes that handler-side effects are complete before PostToolUse work can await.
    pub(crate) fn handler_completed(&self) {
        let _ = self.state.compare_exchange(
            HANDLER_RUNNING,
            HANDLER_COMPLETED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Whether cancellation was linearized while a cooperative handler was still running.
    pub(crate) fn cooperative_cancellation_won(&self) -> bool {
        self.state.load(Ordering::Acquire) == COOPERATIVE_CANCELLING
    }

    /// Claims the one terminal lifecycle notification, regardless of the current nonterminal phase.
    pub(crate) fn try_claim_terminal(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if current == TERMINAL_CLAIMED {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                TERMINAL_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Linearizes cancellation against handler admission and completion.
    pub(crate) fn cancellation_disposition(
        &self,
        handler_is_cooperative: bool,
    ) -> CancellationDisposition {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            match current {
                TERMINAL_CLAIMED | HANDLER_COMPLETED | COOPERATIVE_CANCELLING => {
                    return CancellationDisposition::AwaitDispatch;
                }
                HANDLER_RUNNING if handler_is_cooperative => {
                    match self.state.compare_exchange_weak(
                        HANDLER_RUNNING,
                        COOPERATIVE_CANCELLING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return CancellationDisposition::AwaitDispatch,
                        Err(observed) => current = observed,
                    }
                }
                PENDING | HANDLER_RUNNING => {
                    match self.state.compare_exchange_weak(
                        current,
                        TERMINAL_CLAIMED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return CancellationDisposition::AbortDispatch,
                        Err(observed) => current = observed,
                    }
                }
                _ => unreachable!("invalid tool-call lifecycle state {current}"),
            }
        }
    }
}

#[cfg(test)]
#[path = "call_lifecycle_tests.rs"]
mod tests;
