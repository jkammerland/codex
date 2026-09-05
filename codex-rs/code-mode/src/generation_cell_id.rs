use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::WaitOutcome;
use uuid::Uuid;

const STALE_GENERATION_ERROR: &str = "cell belongs to a stale code-mode host generation";

/// Starts each logical session in a distinct public cell-ID namespace.
///
/// Generation one remains reserved for decoding historical raw handles. New sessions use a
/// randomized seed and increment it on reconnect, so a persisted handle cannot address a reused
/// host-owned ID after rollout resume. Keeping the seed below `i64::MAX` also leaves more than
/// enough room for reconnect generations without overflowing the fixed-width public prefix.
pub(crate) fn initial_session_generation() -> u64 {
    let bits = Uuid::new_v4().as_u128();
    let mixed = (bits as u64) ^ ((bits >> u64::BITS) as u64);
    (mixed & (i64::MAX as u64)).max(2)
}

/// Maps a host-owned ID into the public identity space for its connection generation.
///
/// First-generation IDs keep their historical representation unless they
/// resemble a reserved generation envelope. Escaping those IDs prevents a raw
/// first-generation ID such as `g2:42` from colliding with generation two's
/// public identity for raw ID `42`.
pub(crate) fn encode_public_cell_id(generation: u64, host_cell_id: &str) -> CellId {
    debug_assert!(!host_cell_id.is_empty());
    debug_assert!(host_cell_id.len() <= codex_code_mode_protocol::grpc::MAX_IDENTIFIER_BYTES);
    if generation == 1 && !has_generation_envelope(host_cell_id) {
        CellId::new(host_cell_id.to_string())
    } else {
        CellId::new(format!("g{generation}:{host_cell_id}"))
    }
}

/// Recovers a host-owned ID only when the public identity belongs to `generation`.
pub(crate) fn decode_public_cell_id(
    generation: u64,
    public_cell_id: &CellId,
) -> Result<&str, String> {
    let value = public_cell_id.as_str();
    let host_cell_id = if generation == 1 {
        if !has_generation_envelope(value) {
            value
        } else {
            value
                .strip_prefix("g1:")
                .filter(|host_cell_id| has_generation_envelope(host_cell_id))
                .ok_or_else(|| STALE_GENERATION_ERROR.to_string())?
        }
    } else {
        let prefix = format!("g{generation}:");
        value
            .strip_prefix(&prefix)
            .ok_or_else(|| STALE_GENERATION_ERROR.to_string())?
    };
    if host_cell_id.is_empty()
        || host_cell_id.len() > codex_code_mode_protocol::grpc::MAX_IDENTIFIER_BYTES
    {
        return Err(STALE_GENERATION_ERROR.to_string());
    }
    Ok(host_cell_id)
}

/// Returns a terminal outcome when `cell_id` belongs to another host generation.
pub(crate) fn stale_cell_outcome(generation: u64, cell_id: &CellId) -> Option<WaitOutcome> {
    decode_public_cell_id(generation, cell_id)
        .is_err()
        .then(|| {
            WaitOutcome::MissingCell(RuntimeResponse::Result {
                cell_id: cell_id.clone(),
                content_items: Vec::new(),
                error_text: Some(STALE_GENERATION_ERROR.to_string()),
                code_mode_host_duration: None,
            })
        })
}

fn has_generation_envelope(value: &str) -> bool {
    value
        .strip_prefix('g')
        .and_then(|value| value.split_once(':'))
        .is_some_and(|(generation, _)| {
            !generation.is_empty() && generation.bytes().all(|byte| byte.is_ascii_digit())
        })
}
