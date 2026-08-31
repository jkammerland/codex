use super::*;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicUsize;

#[test]
fn closed_or_cancelled_cells_cannot_submit_nested_tools() {
    let cell_id = CellId::new("cell-1".to_string());
    let dispatch_gates = Mutex::new(HashMap::from([(
        cell_id.clone(),
        CellDispatchGate {
            ready: watch::channel(true).0,
            host: None,
            originating_item_id: None,
        },
    )]));
    let dispatch_count = AtomicUsize::new(0);
    let cancellation_token = CancellationToken::new();
    let dispatch = || dispatch_count.fetch_add(1, Ordering::AcqRel);

    assert_eq!(
        with_active_cell_dispatch(&dispatch_gates, &cell_id, &cancellation_token, dispatch,),
        Some(0)
    );
    remove_dispatch_gate(&dispatch_gates, &cell_id);
    assert_eq!(
        with_active_cell_dispatch(&dispatch_gates, &cell_id, &cancellation_token, dispatch,),
        None
    );

    dispatch_gate(&dispatch_gates, &cell_id);
    cancellation_token.cancel();
    assert_eq!(
        with_active_cell_dispatch(&dispatch_gates, &cell_id, &cancellation_token, dispatch,),
        None
    );
    assert_eq!(dispatch_count.load(Ordering::Acquire), 1);
}
