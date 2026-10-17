use std::sync::Weak;

use anyhow::Result;
use codex_app_server_protocol::*;
use pretty_assertions::assert_eq;

use super::HostEvent;
use crate::PreparedTeamHost;
use crate::TeamHostService;

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "fences background dispatch while exercising the production lifecycle handler under its ordinary operation lock"
)]
async fn unchanged_board_sync_and_consumed_dispatch_do_not_echo_team_changed() -> Result<()> {
    let home = tempfile::tempdir()?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let service = TeamHostService::new(
        PreparedTeamHost::load(&config, "installation".into()).await?,
        Weak::new(),
    );
    let store = service.enable().await?;
    store
        .apply_control(&TeamControlState {
            coordinator_id: "coordinator".into(),
            revision: 1,
            mode: TeamExecutionMode::Paused,
        })
        .await?;
    let snapshot = TeamBoardSnapshot {
        revision: 4,
        ..Default::default()
    };
    store.put("board_cache", "current", &snapshot).await?;

    // Drain initialization's control event through its observed notification.
    // The operations lock fences its queued dispatch while we test the same
    // production event handler directly, including its post-dispatch tail.
    let mut changed = service.subscribe();
    if *changed.borrow_and_update() == 0 {
        changed.changed().await?;
    }
    let _operation = service.operations.lock().await;
    changed.borrow_and_update();
    let revision = *changed.borrow();
    service.handle_event(HostEvent::Dispatch).await?;
    assert_eq!(*changed.borrow(), revision);
    drop(_operation);

    service
        .sync(TeamSyncParams {
            coordinator_id: "coordinator".into(),
            directory: Vec::new(),
            board: Vec::new(),
            board_state: snapshot,
            cancellation_adapters: Vec::new(),
        })
        .await?;
    let _operation = service.operations.lock().await;
    service.handle_event(HostEvent::Dispatch).await?;
    assert_eq!(*changed.borrow(), revision);
    Ok(())
}
