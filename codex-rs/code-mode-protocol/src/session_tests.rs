use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::CellId;
use super::MAX_CELL_ID_BYTES;
use super::StartedCell;

#[test]
fn cell_id_admission_is_bounded_in_utf8_bytes() {
    let exact = format!("{}ab", "🦀".repeat(MAX_CELL_ID_BYTES / 4));
    let oversized = format!("{exact}x");

    assert_eq!(exact.len(), MAX_CELL_ID_BYTES);
    assert_eq!(oversized.len(), MAX_CELL_ID_BYTES + 1);
    assert_eq!(CellId::try_new(exact.clone()), Ok(CellId::new(exact)));
    assert_eq!(
        CellId::try_new(oversized.clone()),
        Err(format!(
            "cell ID is {} bytes; maximum is {MAX_CELL_ID_BYTES} bytes",
            oversized.len()
        ))
    );
    assert_eq!(
        serde_json::from_value::<CellId>(serde_json::Value::String(oversized.clone()))
            .expect_err("serialized oversized cell ID should be rejected")
            .to_string(),
        format!(
            "cell ID is {} bytes; maximum is {MAX_CELL_ID_BYTES} bytes",
            oversized.len()
        )
    );
}

#[test]
fn public_cell_id_cap_includes_the_longest_generation_prefix() {
    let public_id = format!(
        "g{}:{}",
        u64::MAX,
        "x".repeat(crate::grpc::MAX_IDENTIFIER_BYTES)
    );

    assert_eq!(public_id.len(), MAX_CELL_ID_BYTES);
    assert_eq!(
        CellId::try_new(public_id.clone()),
        Ok(CellId::new(public_id))
    );
}

#[tokio::test]
async fn started_cell_preserves_remote_initial_response_errors() {
    let (response_tx, response_rx) = oneshot::channel();
    response_tx
        .send(Err("remote runtime failed".to_string()))
        .expect("initial response receiver should be open");
    let started = StartedCell::from_result_receiver(CellId::new("1".to_string()), response_rx);

    assert_eq!(
        started.initial_response().await,
        Err("remote runtime failed".to_string())
    );
}
