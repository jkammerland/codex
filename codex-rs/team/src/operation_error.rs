//! Definitive rejection is distinct from an uncertain submission or commit.

/// The coordinator durably rejected a new Board operation before writing Board effects.
/// Callers may keep the draft editable and submit corrected content with a new ID.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamOperationRejected {
    pub operation_id: String,
    pub reason: String,
}

impl std::fmt::Display for TeamOperationRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "operation {} was rejected: {}",
            self.operation_id, self.reason
        )
    }
}

impl std::error::Error for TeamOperationRejected {}
