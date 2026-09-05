use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use pretty_assertions::assert_eq;

use super::RemoteSession;
use super::public_cell_id;
use super::remote_cell_id;

fn session(generation: u64) -> RemoteSession {
    RemoteSession {
        id: SessionId::new("session").expect("valid session ID"),
        generation,
    }
}

#[test]
fn serialized_host_ids_are_collision_free_across_generation_rollover() {
    let first_raw_id = WireCellId::new("g2:42");
    let second_raw_id = WireCellId::new("42");
    let first_public_id = public_cell_id(/*generation*/ 1, &first_raw_id);
    let second_public_id = public_cell_id(/*generation*/ 2, &second_raw_id);

    assert_eq!(first_public_id, CellId::new("g1:g2:42".to_string()));
    assert_eq!(second_public_id, CellId::new("g2:42".to_string()));
    assert_ne!(first_public_id, second_public_id);
    assert_eq!(
        remote_cell_id(&session(/*generation*/ 1), &first_public_id),
        Ok(first_raw_id)
    );
    for operation in ["wait", "terminate"] {
        assert_eq!(
            remote_cell_id(&session(/*generation*/ 2), &first_public_id),
            Err("cell g1:g2:42 belongs to a stale code-mode host generation".to_string()),
            "stale public ID reached serialized {operation} routing"
        );
    }
    assert_eq!(
        remote_cell_id(&session(/*generation*/ 2), &second_public_id),
        Ok(second_raw_id)
    );
}

#[test]
fn first_generation_keeps_non_reserved_opaque_ids() {
    let raw_id = WireCellId::new("graphics:1");
    let public_id = public_cell_id(/*generation*/ 1, &raw_id);

    assert_eq!(public_id, CellId::new("graphics:1".to_string()));
    assert_eq!(
        remote_cell_id(&session(/*generation*/ 1), &public_id),
        Ok(raw_id)
    );
}
