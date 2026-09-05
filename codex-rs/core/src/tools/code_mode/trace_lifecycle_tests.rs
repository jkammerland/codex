use codex_code_mode::CellId;
use pretty_assertions::assert_eq;

use super::CellTraceProvenance;
use super::MAX_CELL_TRACE_PROVENANCE;

#[test]
fn terminal_trace_is_claimed_once_only_after_a_start() {
    let mut provenance = CellTraceProvenance::default();
    let started = CellId::new("started".to_string());
    let unknown = CellId::new("unknown".to_string());

    assert!(!provenance.claim_end(&unknown));
    provenance.record_started(&started);
    assert!(provenance.claim_end(&started));
    assert!(!provenance.claim_end(&started));
}

#[test]
fn provenance_is_bounded_and_cell_id_reuse_does_not_remove_the_new_start() {
    let mut provenance = CellTraceProvenance::default();
    let reused = CellId::new("reused".to_string());
    provenance.record_started(&reused);
    provenance.record_started(&reused);

    for index in 0..MAX_CELL_TRACE_PROVENANCE - 1 {
        provenance.record_started(&CellId::new(format!("cell-{index}")));
    }

    assert_eq!(provenance.entries.len(), MAX_CELL_TRACE_PROVENANCE);
    assert_eq!(provenance.order.len(), MAX_CELL_TRACE_PROVENANCE);
    assert!(provenance.claim_end(&reused));
}

#[test]
fn repeated_cell_id_reuse_bounds_order_and_preserves_the_newest_start() {
    let mut provenance = CellTraceProvenance::default();
    let reused = CellId::new("reused".to_string());

    for _ in 0..MAX_CELL_TRACE_PROVENANCE * 3 {
        provenance.record_started(&reused);
    }

    assert_eq!(provenance.entries.len(), 1);
    assert_eq!(provenance.order.len(), MAX_CELL_TRACE_PROVENANCE);
    assert!(provenance.claim_end(&reused));
    assert!(!provenance.claim_end(&reused));
}
