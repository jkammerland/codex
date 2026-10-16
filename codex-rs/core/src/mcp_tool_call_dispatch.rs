//! Pending admission is cancellable; an admitted external call keeps its owner.

use codex_extension_api::ExecutionPermit;
use codex_extension_api::RetainedToolResults;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

pub(crate) async fn enter_execution(
    session: &Session,
    cancellation: &CancellationToken,
) -> Result<ExecutionPermit, String> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err("user cancelled MCP tool call".into()),
        execution = session.enter_execution() => Ok(execution),
    }
}

/// This synchronous boundary runs after asynchronous preparation. Its shared
/// phase claim prevents interruption from aborting a concurrently admitted call.
pub(crate) fn begin_dispatch(
    turn: &TurnContext,
    call_id: &str,
    cancellation: &CancellationToken,
) -> anyhow::Result<()> {
    anyhow::ensure!(!cancellation.is_cancelled(), "user cancelled MCP tool call");
    if let Some(retained) = turn.extension_data.get::<RetainedToolResults>() {
        anyhow::ensure!(
            retained.begin_dispatch(call_id),
            "user cancelled MCP tool call"
        );
    }
    Ok(())
}
