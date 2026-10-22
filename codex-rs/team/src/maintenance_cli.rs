//! Offline commands never connect to or automatically launch a coordinator.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use clap::Args;
use codex_app_server_protocol::TeamBoardPurgeScope;
use codex_state::SqliteConfig;
use codex_team_store::BoardPurgeCommit;
use codex_team_store::TeamStore;
use codex_utils_absolute_path::AbsolutePathBuf;
use sha2::Digest;
use sha2::Sha256;

#[derive(Debug, Args)]
#[group(id = "purge_scope", required = true, multiple = false)]
pub struct PurgeScopeArgs {
    #[arg(long)]
    conversation: Option<String>,
    #[arg(long)]
    topic: Option<String>,
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Args)]
pub struct BoardPurgeArgs {
    #[command(flatten)]
    scope: PurgeScopeArgs,
    /// Require exclusive ownership of the stopped coordinator's ledger.
    #[arg(long, required = true)]
    offline: bool,
    /// Permanently remove the previewed content. Omit to preview without writes.
    #[arg(long, requires = "expect_revision")]
    apply: bool,
    #[arg(long, requires = "apply")]
    expect_revision: Option<i64>,
}

impl PurgeScopeArgs {
    fn into_scope(self) -> Result<TeamBoardPurgeScope> {
        if let Some(id) = self.conversation {
            return Ok(TeamBoardPurgeScope::Conversation {
                conversation_id: uuid::Uuid::parse_str(&id)?.to_string(),
            });
        }
        if let Some(topic) = self.topic {
            ensure!(!topic.trim().is_empty(), "topic must not be blank");
            return Ok(TeamBoardPurgeScope::Topic { topic });
        }
        ensure!(self.all, "select a conversation, topic, or the whole Board");
        Ok(TeamBoardPurgeScope::All)
    }
}

pub(crate) async fn run(
    manifest_path: &Path,
    args: BoardPurgeArgs,
    operation_id: Option<String>,
) -> Result<crate::TeamResponse> {
    ensure!(args.offline, "Board maintenance requires --offline");
    let manifest = crate::Manifest::load(manifest_path).await?;
    let state = &manifest.coordinator.state_dir;
    let path = state.join("coordinator.sqlite");
    ensure!(path.is_file(), "coordinator ledger does not exist");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(state.join("coordinator.lock"))
        .context("coordinator ownership lock is missing")?;
    lock.try_lock()
        .context("coordinator is running; stop its service before offline maintenance")?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(state.clone())?);
    let scope = args.scope.into_scope()?;
    if !args.apply {
        let store = TeamStore::open_read_only(&sqlite, &path).await?;
        return Ok(crate::TeamResponse::BoardPurgePreview {
            preview: store.preview_board_purge(&scope).await?,
        });
    }
    let operation_id = uuid::Uuid::parse_str(
        &operation_id.context("--operation-id is required with --apply; retain it for retries")?,
    )?
    .to_string();
    let expected_revision = args
        .expect_revision
        .context("--expect-revision is required with --apply")?;
    let digest = serde_json::to_string(&format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&crate::TeamRequest::BoardPurge {
            scope: scope.clone(),
            expected_revision
        })?)
    ))?;
    let store = TeamStore::open(&sqlite, &path).await?;
    let workers = crate::board_maintenance::purge_workers(&store).await?;
    let operation = store
        .purge_board(BoardPurgeCommit {
            scope,
            expected_revision,
            operation_id,
            digest,
            workers,
        })
        .await?;
    Ok(crate::TeamResponse::Operation {
        operation: crate::TeamOperatorOperation::BoardPurge(operation),
    })
}
