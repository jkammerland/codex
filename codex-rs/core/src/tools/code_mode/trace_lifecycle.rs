use std::collections::HashMap;
use std::collections::VecDeque;

use codex_code_mode::CellId;

const MAX_CELL_TRACE_PROVENANCE: usize = 4_096;

#[derive(Clone, Copy, Eq, PartialEq)]
enum CellTraceState {
    Started,
    Ended,
}

struct CellTraceEntry {
    sequence: u64,
    state: CellTraceState,
}

#[derive(Default)]
pub(super) struct CellTraceProvenance {
    entries: HashMap<CellId, CellTraceEntry>,
    order: VecDeque<(CellId, u64)>,
    next_sequence: u64,
}

impl CellTraceProvenance {
    pub(super) fn record_started(&mut self, cell_id: &CellId) {
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let sequence = self.next_sequence;
        self.entries.insert(
            cell_id.clone(),
            CellTraceEntry {
                sequence,
                state: CellTraceState::Started,
            },
        );
        self.order.push_back((cell_id.clone(), sequence));
        self.enforce_limit();
    }

    pub(super) fn claim_end(&mut self, cell_id: &CellId) -> bool {
        let Some(entry) = self.entries.get_mut(cell_id) else {
            return false;
        };
        if entry.state == CellTraceState::Ended {
            return false;
        }
        entry.state = CellTraceState::Ended;
        true
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn enforce_limit(&mut self) {
        while self.entries.len() > MAX_CELL_TRACE_PROVENANCE
            || self.order.len() > MAX_CELL_TRACE_PROVENANCE
        {
            let Some((cell_id, sequence)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&cell_id)
                .is_some_and(|entry| entry.sequence == sequence)
            {
                self.entries.remove(&cell_id);
            }
        }
    }
}

#[cfg(test)]
#[path = "trace_lifecycle_tests.rs"]
mod tests;
