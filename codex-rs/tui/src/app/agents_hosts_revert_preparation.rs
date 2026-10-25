//! A slow history read or revert belongs to its host, outside terminal event dispatch.

use super::agents_hosts::ConnectionScope;
use super::agents_hosts::HostUpdate;
use super::agents_hosts_revert::PendingHostPromptRevert;
use super::*;

pub(crate) struct PreparedPromptRevert {
    prompt: crate::chatwidget::UserMessage,
    retained_turns: Vec<Turn>,
    reverted: codex_app_server_protocol::ThreadRevertResponse,
}

pub(crate) struct PromptRevertFailure {
    pub(super) message: String,
    pub(super) reload_required: bool,
}

impl App {
    pub(super) async fn start_host_prompt_revert(
        &mut self,
        server: &AppServerSession,
        thread_id: ThreadId,
        nth_user_message: usize,
        selected_cell: Arc<dyn HistoryCell>,
        prompt: crate::chatwidget::UserMessage,
    ) -> Result<()> {
        let Some(channel) = self.thread_event_channels.get(&thread_id) else {
            self.restore_backtrack_prompt_after_revert_error(
                prompt,
                "the selected thread is no longer available",
            );
            return Ok(());
        };
        let (start_item, loaded_tail, latest_turn_id) = {
            let store = channel.store.lock().await;
            (
                store.turns.iter().find_map(|turn| {
                    turn.items
                        .first()
                        .map(|item| (turn.id.clone(), item.id().to_string()))
                }),
                store.turns.last().map(|turn| turn.id.clone()),
                store.latest_turn_id.clone(),
            )
        };
        self.ensure_initial_agents_host(server);
        let scope = self.agents_hosts.scope();
        let request_id = Uuid::new_v4();
        self.pending_host_prompt_reverts.insert(
            thread_id,
            PendingHostPromptRevert {
                request_id,
                prompt: prompt.clone(),
                retained_turns: Vec::new(),
                reverted: None,
                error: None,
                selected_cell,
                notified: false,
            },
        );
        self.chat_widget
            .set_queue_autosend_suppressed(/*suppressed*/ true);
        let mut server = server.request_copy();
        let sender = self.app_event_tx.unscoped();
        self.agents_hosts.spawn(async move {
            let selection = prepare_retained_history(&mut server, thread_id, nth_user_message, prompt, start_item, loaded_tail, latest_turn_id).await;
            let result = match selection {
                Ok((before_turn_id, retained_turns, prompt)) => {
                    match server.revert_thread(thread_id, before_turn_id, &retained_turns).await {
                        Ok(reverted) => Ok(PreparedPromptRevert { prompt, retained_turns, reverted }),
                        Err(error) => {
                            let unchanged = matches!(&error, codex_app_server_client::TypedRequestError::Server { source, .. }
                                if matches!(source.code, -32602..=-32600));
                            Err(PromptRevertFailure {
                                message: if unchanged { format!("Could not edit this prompt: {error}") }
                                    else { "Prompt edit could not be confirmed. Resume this task to reload its history.".into() },
                                reload_required: !unchanged,
                            })
                        }
                    }
                }
                Err(error) => Err(PromptRevertFailure { message: format!("Could not edit this prompt: {error}"), reload_required: false }),
            };
            sender.send(AppEvent::AgentsHostUpdate(Box::new(HostUpdate::PromptRevertPrepared { scope, request_id, result })));
        });
        Ok(())
    }

    pub(super) async fn finish_host_prompt_revert_preparation(
        &mut self,
        scope: ConnectionScope,
        request_id: Uuid,
        result: std::result::Result<PreparedPromptRevert, PromptRevertFailure>,
    ) -> Result<()> {
        if !self.agents_hosts.accepts(&scope) {
            return Ok(());
        }
        let pending = if scope.host == self.agents_hosts.foreground {
            &mut self.pending_host_prompt_reverts
        } else {
            #[expect(
                clippy::expect_used,
                reason = "The scope was accepted above and no host removal or await intervenes."
            )]
            let host = self
                .agents_hosts
                .hosts
                .get_mut(&scope.host)
                .expect("host exists");
            &mut host.state.pending_host_prompt_reverts
        };
        let Some((&thread_id, pending)) = pending
            .iter_mut()
            .find(|(_, pending)| pending.request_id == request_id)
        else {
            return Ok(());
        };
        match result {
            Ok(prepared) => {
                pending.prompt = prepared.prompt;
                pending.retained_turns = prepared.retained_turns;
                pending.reverted = Some(prepared.reverted);
            }
            Err(error) => pending.error = Some(error),
        }
        if scope.host == self.agents_hosts.foreground {
            self.complete_host_prompt_revert(thread_id).await?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare_retained_history(
    server: &mut AppServerSession,
    thread_id: ThreadId,
    nth_user_message: usize,
    mut prompt: crate::chatwidget::UserMessage,
    start_item: Option<(String, String)>,
    loaded_tail: Option<String>,
    latest_turn_id: Option<String>,
) -> Result<(String, Vec<Turn>, crate::chatwidget::UserMessage)> {
    let mut thread = server
        .thread_read(thread_id, /*include_turns*/ false)
        .await?;
    server
        .hydrate_initial_thread_history(
            &mut thread,
            /*turn_cursor*/ None,
            /*item_cursor*/ None,
            /*config*/ None,
            /*local_settings*/ None,
            start_item
                .as_ref()
                .map(|(turn_id, _)| turn_id)
                .or(loaded_tail.as_ref())
                .map_or(
                    crate::app_server_session::HistoryHydrationScope::Complete,
                    |turn_id| {
                        crate::app_server_session::HistoryHydrationScope::ThroughTurn(turn_id)
                    },
                ),
        )
        .await?;
    if thread.turns.last().map(|turn| &turn.id) != latest_turn_id.as_ref() {
        color_eyre::eyre::bail!(
            "thread history changed; reload the session before editing this prompt"
        );
    }
    let start_item = start_item.or_else(|| {
        loaded_tail.as_ref().and_then(|tail| {
            thread
                .turns
                .iter()
                .position(|turn| &turn.id == tail)
                .and_then(|index| {
                    thread.turns[index + 1..].iter().find_map(|turn| {
                        turn.items
                            .first()
                            .map(|item| (turn.id.clone(), item.id().to_string()))
                    })
                })
        })
    });
    let before_turn_id = crate::app_backtrack::backtrack_revert_before_turn_id(
        &thread.turns,
        start_item.as_ref(),
        nth_user_message,
        &mut prompt,
    )?;
    let cut = thread
        .turns
        .iter()
        .position(|turn| turn.id == before_turn_id)
        .ok_or_else(|| color_eyre::eyre::eyre!("selected turn disappeared"))?;
    thread.turns.truncate(cut);
    if let Some((turn_id, item_id)) = &start_item {
        for turn in &mut thread.turns {
            if &turn.id == turn_id {
                if let Some(index) = turn.items.iter().position(|item| item.id() == item_id) {
                    turn.items.drain(..index);
                }
                break;
            }
            turn.items.clear();
        }
    }
    Ok((before_turn_id, thread.turns, prompt))
}
