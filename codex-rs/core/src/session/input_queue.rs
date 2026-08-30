use crate::context_manager::estimate_item_token_count;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::Gauge;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;

static PENDING_MAILBOX_MESSAGES: Gauge = Gauge::new("core.mailbox.pending");
const MAX_MAILBOX_ITEMS_PER_TURN: usize = 64;
pub(super) const MAX_MAILBOX_MODEL_TOKENS_PER_TURN: usize = 9_000;
pub(super) const MAX_MAILBOX_MODEL_TOKENS_PER_ITEM: usize = 1_000;
const MAX_LEGACY_MAILBOX_MODEL_TOKENS_PER_ITEM: usize = 10_000;
const INTER_AGENT_MODEL_CONTENT_MAX_TOKENS: usize = 768;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MailboxModelSizeLimit {
    Current,
    LegacyRecovery,
}

impl MailboxModelSizeLimit {
    fn max_tokens(self) -> usize {
        match self {
            Self::Current => MAX_MAILBOX_MODEL_TOKENS_PER_ITEM,
            Self::LegacyRecovery => MAX_LEGACY_MAILBOX_MODEL_TOKENS_PER_ITEM,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct PreparedMailboxModelCommunication {
    pub(super) communication: InterAgentCommunication,
    pub(super) unstamped_communication: InterAgentCommunication,
    model_tokens: usize,
}

pub(super) fn mailbox_model_communication(
    communication: &InterAgentCommunication,
) -> InterAgentCommunication {
    let mut model_communication = communication.clone();
    model_communication.author =
        crate::session_prefix::bounded_completion_agent_path(&communication.author);
    model_communication.recipient =
        crate::session_prefix::bounded_completion_agent_path(&communication.recipient);
    model_communication.other_recipients = communication
        .other_recipients
        .iter()
        .map(crate::session_prefix::bounded_completion_agent_path)
        .collect();
    model_communication.content = truncate_text(
        &communication.content,
        TruncationPolicy::Tokens(INTER_AGENT_MODEL_CONTENT_MAX_TOKENS),
    );
    model_communication
}

#[cfg(test)]
pub(super) fn mailbox_model_token_estimate(communication: &InterAgentCommunication) -> usize {
    let model_communication = mailbox_model_communication(communication);
    stamped_mailbox_model_communication(model_communication, "00000000-0000-0000-0000-000000000000")
        .model_tokens
}

fn stamped_mailbox_model_communication(
    mut communication: InterAgentCommunication,
    turn_id: &str,
) -> PreparedMailboxModelCommunication {
    let unstamped_communication = communication.clone();
    let metadata = communication
        .internal_chat_message_metadata_passthrough
        .get_or_insert_with(InternalChatMessageMetadataPassthrough::default);
    if metadata.turn_id.as_deref() != Some(turn_id) {
        metadata.turn_id = Some(turn_id.to_string());
        metadata.create_time = Some(super::Session::response_item_create_time());
    } else {
        metadata
            .create_time
            .get_or_insert_with(super::Session::response_item_create_time);
    }
    let model_tokens = usize::try_from(estimate_item_token_count(
        &communication.to_model_input_item(),
    ))
    .unwrap_or(usize::MAX);
    PreparedMailboxModelCommunication {
        communication,
        unstamped_communication,
        model_tokens,
    }
}

pub(super) fn prepare_mailbox_model_communication(
    communication: &InterAgentCommunication,
    turn_id: &str,
    size_limit: MailboxModelSizeLimit,
) -> io::Result<PreparedMailboxModelCommunication> {
    let model_communication = match size_limit {
        MailboxModelSizeLimit::Current => mailbox_model_communication(communication),
        MailboxModelSizeLimit::LegacyRecovery => communication.clone(),
    };
    prepare_stored_mailbox_model_communication(model_communication, turn_id, size_limit)
}

fn prepare_stored_mailbox_model_communication(
    model_communication: InterAgentCommunication,
    turn_id: &str,
    size_limit: MailboxModelSizeLimit,
) -> io::Result<PreparedMailboxModelCommunication> {
    let prepared = stamped_mailbox_model_communication(model_communication, turn_id);
    validate_stored_mailbox_model_communication(&prepared.communication, size_limit)?;
    Ok(prepared)
}

pub(super) fn validate_stored_mailbox_model_communication(
    communication: &InterAgentCommunication,
    size_limit: MailboxModelSizeLimit,
) -> io::Result<()> {
    let model_tokens = usize::try_from(estimate_item_token_count(
        &communication.to_model_input_item(),
    ))
    .unwrap_or(usize::MAX);
    let max_tokens = size_limit.max_tokens();
    if model_tokens > max_tokens {
        let id = communication
            .id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "without an ID".to_string());
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "inter-agent communication {id} is too large for one model-context item ({model_tokens} estimated tokens; maximum {max_tokens})"
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DurableMailClaim {
    pub(crate) communication_id: ResponseItemId,
    pub(crate) fingerprint: String,
    pub(crate) model_communication: InterAgentCommunication,
    model_tokens: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InflightMailCommit {
    Reserved(Box<DurableMailClaim>),
    Indeterminate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableMailRetryStatus {
    Delivered,
    Pending { trigger_turn: bool, persisted: bool },
    InFlight,
    Missing,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MailboxClaimSnapshot {
    pub(crate) selected_count: usize,
    pub(crate) durable_mail: Vec<DurableMailClaim>,
}

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    FunctionCallOutput(ResponseItem),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(InterAgentCommunication),
}

mod turn_input_response_item {
    use super::ResponseItem;
    use super::ResponseItemEnvelope;
    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::ser::Error as _;

    pub(super) fn serialize<S>(
        item: &ResponseItemEnvelope,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if item.metadata.is_some() {
            return Err(S::Error::custom(
                "annotated response items cannot cross the turn-input serialization boundary",
            ));
        }
        item.item.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<ResponseItemEnvelope, D::Error>
    where
        D: Deserializer<'de>,
    {
        ResponseItem::deserialize(deserializer).map(ResponseItemEnvelope::new)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputQueueActivity {
    Mailbox,
    Steer,
}

/// Turn-local pending input storage owned by the input queue flow.
#[derive(Default)]
pub(crate) struct TurnInputQueue {
    pub(super) items: Vec<TurnInput>,
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    mailbox: Mutex<MailboxState>,
}

#[derive(Default)]
struct MailboxState {
    pending_mails: VecDeque<PendingMailboxCommunication>,
    inflight_mails: VecDeque<InflightMailboxCommunication>,
    recorded_durable_mail: HashMap<ResponseItemId, RecordedDurableMail>,
}

struct RecordedDurableMail {
    fingerprint: String,
    delivered: bool,
}

impl MailboxState {
    fn record_durable_mail(&mut self, id: ResponseItemId, fingerprint: String, delivered: bool) {
        self.recorded_durable_mail.insert(
            id,
            RecordedDurableMail {
                fingerprint,
                delivered,
            },
        );
    }
}

struct PendingMailboxCommunication {
    communication: InterAgentCommunication,
    model_communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    fingerprint: Option<String>,
    intended_turn_id: Option<String>,
    model_tokens: usize,
    size_limit: MailboxModelSizeLimit,
    persisted: bool,
    _diagnostics_guard: GaugeGuard,
}

#[derive(Clone)]
struct InflightMailboxCommunication {
    communication: InterAgentCommunication,
    model_communication: InterAgentCommunication,
    start_options: TurnStartOptions,
    fingerprint: String,
    intended_turn_id: Option<String>,
    claimed_turn_id: String,
    model_tokens: usize,
    size_limit: MailboxModelSizeLimit,
    commit_state: InflightMailCommitState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InflightMailCommitState {
    Reserved,
    CommitIndeterminate,
}

fn deliverable_mail_count(pending_mails: &VecDeque<PendingMailboxCommunication>) -> usize {
    let mut count = 0;
    let mut model_tokens = 0usize;
    for mail in pending_mails
        .iter()
        .take_while(|mail| mail.persisted)
        .take(MAX_MAILBOX_ITEMS_PER_TURN)
    {
        let next_model_tokens = model_tokens.saturating_add(mail.model_tokens);
        let legacy_single_item = count == 0
            && mail.size_limit == MailboxModelSizeLimit::LegacyRecovery
            && next_model_tokens <= MAX_LEGACY_MAILBOX_MODEL_TOKENS_PER_ITEM;
        if next_model_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_TURN && !legacy_single_item {
            break;
        }
        count += 1;
        model_tokens = next_model_tokens;
    }
    count
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            mailbox: Mutex::new(MailboxState::default()),
        }
    }

    pub(crate) async fn restore_recorded_durable_mail(
        &self,
        id: ResponseItemId,
        fingerprint: String,
        _claimed_turn_id: Option<String>,
    ) {
        self.mailbox
            .lock()
            .await
            .record_durable_mail(id, fingerprint, /*delivered*/ true);
    }

    pub(super) async fn restore_claimed_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
        fingerprint: String,
        claimed_turn_id: String,
        size_limit: MailboxModelSizeLimit,
    ) -> io::Result<bool> {
        let Some(id) = communication.id.clone() else {
            return Ok(false);
        };
        let prepared =
            prepare_mailbox_model_communication(&communication, &claimed_turn_id, size_limit)?;
        let mut mailbox = self.mailbox.lock().await;
        if mailbox
            .pending_mails
            .iter()
            .any(|pending| pending.communication.id.as_ref() == Some(&id))
        {
            return Ok(false);
        }
        mailbox
            .pending_mails
            .push_back(PendingMailboxCommunication {
                communication,
                model_communication: prepared.unstamped_communication,
                start_options,
                fingerprint: Some(fingerprint.clone()),
                intended_turn_id: Some(claimed_turn_id.clone()),
                model_tokens: prepared.model_tokens,
                size_limit,
                persisted: true,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        mailbox.record_durable_mail(id, fingerprint, /*delivered*/ false);
        drop(mailbox);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        Ok(true)
    }

    pub(crate) async fn recorded_durable_mail_fingerprint(
        &self,
        id: &ResponseItemId,
    ) -> Option<String> {
        self.mailbox
            .lock()
            .await
            .recorded_durable_mail
            .get(id)
            .map(|mail| mail.fingerprint.clone())
    }

    pub(crate) async fn pending_durable_mail_fingerprint(
        &self,
        id: &ResponseItemId,
    ) -> Option<String> {
        let mailbox = self.mailbox.lock().await;
        mailbox
            .pending_mails
            .iter()
            .find(|mail| mail.communication.id.as_ref() == Some(id))
            .and_then(|mail| mail.fingerprint.clone())
            .or_else(|| {
                mailbox
                    .inflight_mails
                    .iter()
                    .find(|mail| mail.communication.id.as_ref() == Some(id))
                    .map(|mail| mail.fingerprint.clone())
            })
    }

    #[cfg(test)]
    pub(crate) async fn durable_mail_was_consumed(&self, id: &ResponseItemId) -> bool {
        self.mailbox
            .lock()
            .await
            .recorded_durable_mail
            .get(id)
            .is_some_and(|mail| mail.delivered)
    }

    pub(crate) async fn durable_mail_retry_status(
        &self,
        id: &ResponseItemId,
    ) -> DurableMailRetryStatus {
        let mailbox = self.mailbox.lock().await;
        if mailbox
            .recorded_durable_mail
            .get(id)
            .is_some_and(|mail| mail.delivered)
        {
            DurableMailRetryStatus::Delivered
        } else if let Some(mail) = mailbox
            .pending_mails
            .iter()
            .find(|mail| mail.communication.id.as_ref() == Some(id))
        {
            DurableMailRetryStatus::Pending {
                trigger_turn: mail.communication.trigger_turn,
                persisted: mail.persisted,
            }
        } else if mailbox
            .inflight_mails
            .iter()
            .any(|mail| mail.communication.id.as_ref() == Some(id))
        {
            DurableMailRetryStatus::InFlight
        } else {
            DurableMailRetryStatus::Missing
        }
    }

    pub(super) async fn prepare_persisted_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        prepared: PreparedMailboxModelCommunication,
        start_options: TurnStartOptions,
        fingerprint: String,
        intended_turn_id: String,
        size_limit: MailboxModelSizeLimit,
    ) -> io::Result<bool> {
        let Some(id) = communication.id.as_ref() else {
            return Ok(false);
        };
        let mut mailbox = self.mailbox.lock().await;
        if let Some(pending) = mailbox
            .pending_mails
            .iter()
            .find(|pending| pending.communication.id.as_ref() == Some(id))
        {
            return Ok(pending.fingerprint.as_deref() == Some(fingerprint.as_str()));
        }
        mailbox
            .pending_mails
            .push_back(PendingMailboxCommunication {
                communication,
                model_communication: prepared.unstamped_communication,
                start_options,
                fingerprint: Some(fingerprint),
                intended_turn_id: Some(intended_turn_id),
                model_tokens: prepared.model_tokens,
                size_limit,
                persisted: false,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        Ok(true)
    }

    pub(crate) async fn mark_mailbox_communication_persisted(&self, id: &ResponseItemId) -> bool {
        let mut mailbox = self.mailbox.lock().await;
        let Some(pending) = mailbox
            .pending_mails
            .iter_mut()
            .find(|pending| pending.communication.id.as_ref() == Some(id))
        else {
            return false;
        };
        pending.persisted = true;
        if let Some(fingerprint) = pending.fingerprint.clone() {
            mailbox.record_durable_mail(id.clone(), fingerprint, /*delivered*/ false);
        }
        drop(mailbox);
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        true
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let has_pending_steer = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.has_pending_input()
        } else {
            false
        };
        let pending_activity = if has_pending_steer {
            Some(InputQueueActivity::Steer)
        } else if self.has_deliverable_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: InterAgentCommunication,
        start_options: TurnStartOptions,
    ) -> io::Result<()> {
        let mut sized_communication = communication.clone();
        sized_communication
            .id
            .get_or_insert_with(|| ResponseItemId::new("msg"));
        let prepared = prepare_mailbox_model_communication(
            &sized_communication,
            "00000000-0000-0000-0000-000000000000",
            MailboxModelSizeLimit::Current,
        )?;
        self.mailbox
            .lock()
            .await
            .pending_mails
            .push_back(PendingMailboxCommunication {
                communication,
                model_communication: prepared.unstamped_communication,
                start_options,
                fingerprint: None,
                intended_turn_id: None,
                model_tokens: prepared.model_tokens,
                size_limit: MailboxModelSizeLimit::Current,
                persisted: true,
                _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
            });
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        Ok(())
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        !self.mailbox.lock().await.pending_mails.is_empty()
    }

    async fn has_deliverable_mailbox_items(&self) -> bool {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .front()
            .is_some_and(|mail| mail.persisted)
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        self.mailbox
            .lock()
            .await
            .pending_mails
            .iter()
            .take_while(|mail| mail.persisted)
            .any(|mail| mail.communication.trigger_turn)
    }

    pub(crate) async fn pending_mailbox_turn_id(&self) -> Option<String> {
        let mailbox = self.mailbox.lock().await;
        let deliverable_count = deliverable_mail_count(&mailbox.pending_mails);
        mailbox
            .pending_mails
            .iter()
            .take(deliverable_count)
            .find_map(|mail| mail.intended_turn_id.clone())
    }

    pub(crate) async fn pending_mailbox_claim_snapshot(
        &self,
        turn_id: &str,
    ) -> io::Result<MailboxClaimSnapshot> {
        let mailbox = self.mailbox.lock().await;
        let mut selected_count = 0;
        let mut selected_model_tokens = 0usize;
        let mut durable_mail = Vec::new();
        for mail in mailbox
            .pending_mails
            .iter()
            .take_while(|mail| mail.persisted)
            .take(MAX_MAILBOX_ITEMS_PER_TURN)
        {
            let prepared = prepare_stored_mailbox_model_communication(
                mail.model_communication.clone(),
                turn_id,
                mail.size_limit,
            )?;
            let next_model_tokens = selected_model_tokens.saturating_add(prepared.model_tokens);
            let legacy_single_item = selected_count == 0
                && mail.size_limit == MailboxModelSizeLimit::LegacyRecovery
                && next_model_tokens <= MAX_LEGACY_MAILBOX_MODEL_TOKENS_PER_ITEM;
            if next_model_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_TURN && !legacy_single_item {
                break;
            }
            if let (Some(communication_id), Some(fingerprint)) =
                (mail.communication.id.clone(), mail.fingerprint.clone())
            {
                durable_mail.push(DurableMailClaim {
                    communication_id,
                    fingerprint,
                    model_communication: prepared.communication,
                    model_tokens: prepared.model_tokens,
                });
            }
            selected_count += 1;
            selected_model_tokens = next_model_tokens;
        }
        Ok(MailboxClaimSnapshot {
            selected_count,
            durable_mail,
        })
    }

    #[cfg(test)]
    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        self.drain_mailbox_input_items_inner(/*claim*/ None)
            .await
            .expect("unclaimed mailbox drain cannot conflict")
    }

    pub(crate) async fn drain_claimed_mailbox_input_items(
        &self,
        snapshot: &MailboxClaimSnapshot,
        turn_id: &str,
    ) -> Option<(Vec<TurnInput>, TurnStartOptions)> {
        self.drain_mailbox_input_items_inner(Some((snapshot, turn_id)))
            .await
    }

    async fn drain_mailbox_input_items_inner(
        &self,
        claim: Option<(&MailboxClaimSnapshot, &str)>,
    ) -> Option<(Vec<TurnInput>, TurnStartOptions)> {
        let pending_mails = {
            let mut mailbox = self.mailbox.lock().await;
            let selected_count = match claim {
                Some((snapshot, _)) => snapshot.selected_count,
                None => deliverable_mail_count(&mailbox.pending_mails),
            };
            if let Some((snapshot, _turn_id)) = claim {
                if selected_count > deliverable_mail_count(&mailbox.pending_mails) {
                    return None;
                }
                let current_claims = mailbox
                    .pending_mails
                    .iter()
                    .take(selected_count)
                    .filter_map(|mail| {
                        Some((mail.communication.id.clone()?, mail.fingerprint.clone()?))
                    })
                    .collect::<Vec<_>>();
                if current_claims
                    != snapshot
                        .durable_mail
                        .iter()
                        .map(|claim| (claim.communication_id.clone(), claim.fingerprint.clone()))
                        .collect::<Vec<_>>()
                {
                    return None;
                }
            }
            let selected = mailbox
                .pending_mails
                .drain(..selected_count)
                .collect::<Vec<_>>();
            if let Some((snapshot, turn_id)) = claim {
                for mail in &selected {
                    let Some(id) = mail.communication.id.clone() else {
                        continue;
                    };
                    let Some(fingerprint) = mail.fingerprint.clone() else {
                        continue;
                    };
                    mailbox.record_durable_mail(id, fingerprint, /*delivered*/ false);
                }
                mailbox
                    .inflight_mails
                    .extend(selected.iter().filter_map(|mail| {
                        let communication_id = mail.communication.id.clone()?;
                        let claim = snapshot
                            .durable_mail
                            .iter()
                            .find(|claim| claim.communication_id == communication_id)?;
                        Some(InflightMailboxCommunication {
                            communication: mail.communication.clone(),
                            model_communication: claim.model_communication.clone(),
                            start_options: mail.start_options.clone(),
                            fingerprint: mail.fingerprint.clone()?,
                            intended_turn_id: mail.intended_turn_id.clone(),
                            claimed_turn_id: turn_id.to_string(),
                            model_tokens: claim.model_tokens,
                            size_limit: mail.size_limit,
                            commit_state: InflightMailCommitState::Reserved,
                        })
                    }));
                debug_assert_eq!(
                    snapshot.selected_count,
                    selected.len(),
                    "the frozen mailbox batch changed while being claimed"
                );
            }
            selected
        };
        // A later follow-up supersedes the earlier choice, including an omitted choice.
        let mut start_options = pending_mails
            .iter()
            .rev()
            .find(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.clone())
            .unwrap_or_default();
        start_options.parent_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| mail.start_options.parent_turn_id.as_deref())
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        start_options.root_turn_id = pending_mails
            .iter()
            .filter(|mail| mail.communication.trigger_turn)
            .map(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
            })
            .reduce(|expected, candidate| expected.filter(|id| candidate == Some(*id)))
            .and_then(|id| id.filter(|id| !id.trim().is_empty()).map(str::to_string));
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        Some((items, start_options))
    }

    pub(crate) async fn inflight_mail_commit(
        &self,
        id: &ResponseItemId,
        turn_id: &str,
    ) -> Option<InflightMailCommit> {
        self.mailbox
            .lock()
            .await
            .inflight_mails
            .iter()
            .find(|mail| {
                mail.communication.id.as_ref() == Some(id) && mail.claimed_turn_id == turn_id
            })
            .and_then(|mail| match mail.commit_state {
                InflightMailCommitState::Reserved => {
                    Some(InflightMailCommit::Reserved(Box::new(DurableMailClaim {
                        communication_id: mail.communication.id.clone()?,
                        fingerprint: mail.fingerprint.clone(),
                        model_communication: mail.model_communication.clone(),
                        model_tokens: mail.model_tokens,
                    })))
                }
                InflightMailCommitState::CommitIndeterminate => {
                    Some(InflightMailCommit::Indeterminate)
                }
            })
    }

    pub(crate) async fn mark_inflight_mail_commit_started(
        &self,
        id: &ResponseItemId,
        turn_id: &str,
    ) -> bool {
        let mut mailbox = self.mailbox.lock().await;
        let Some(mail) = mailbox.inflight_mails.iter_mut().find(|mail| {
            mail.communication.id.as_ref() == Some(id) && mail.claimed_turn_id == turn_id
        }) else {
            return false;
        };
        mail.commit_state = InflightMailCommitState::CommitIndeterminate;
        true
    }

    pub(crate) async fn mark_inflight_mail_delivered(
        &self,
        id: &ResponseItemId,
        turn_id: &str,
    ) -> bool {
        let mut mailbox = self.mailbox.lock().await;
        let Some(index) = mailbox.inflight_mails.iter().position(|mail| {
            mail.communication.id.as_ref() == Some(id) && mail.claimed_turn_id == turn_id
        }) else {
            return false;
        };
        let Some(mail) = mailbox.inflight_mails.remove(index) else {
            return false;
        };
        mailbox.record_durable_mail(id.clone(), mail.fingerprint, /*delivered*/ true);
        true
    }

    pub(crate) async fn release_inflight_mail_for_turn(&self, turn_id: &str) -> usize {
        let mut mailbox = self.mailbox.lock().await;
        let mut retained = VecDeque::new();
        let mut released = VecDeque::new();
        while let Some(mail) = mailbox.inflight_mails.pop_front() {
            if mail.claimed_turn_id == turn_id
                && mail.commit_state == InflightMailCommitState::Reserved
            {
                released.push_back(mail);
            } else {
                retained.push_back(mail);
            }
        }
        mailbox.inflight_mails = retained;
        let released_count = released.len();
        while let Some(mail) = released.pop_back() {
            mailbox
                .pending_mails
                .push_front(PendingMailboxCommunication {
                    communication: mail.communication,
                    model_communication: mail.model_communication,
                    start_options: mail.start_options,
                    fingerprint: Some(mail.fingerprint),
                    intended_turn_id: mail.intended_turn_id,
                    model_tokens: mail.model_tokens,
                    size_limit: mail.size_limit,
                    persisted: true,
                    _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
                });
        }
        drop(mailbox);
        if released_count > 0 {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
        released_count
    }

    pub(crate) async fn turn_state_for_sub_id(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) -> Option<Arc<Mutex<TurnState>>> {
        let active = active_turn.lock().await;
        active.as_ref().and_then(|active_turn| {
            active_turn
                .task
                .as_ref()
                .is_some_and(|task| task.turn_context.sub_id == sub_id)
                .then(|| Arc::clone(&active_turn.turn_state))
        })
    }

    /// Clear any pending waiters and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let mut turn_state = active_turn.turn_state.lock().await;
        turn_state.clear_pending_waiters();
        turn_state.pending_input.items.clear();
    }

    pub(crate) async fn defer_mailbox_delivery_to_next_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        let mut turn_state = turn_state.lock().await;
        // Explicit same-turn work still needs a follow-up. Queue-only child mail does not: keep
        // it pending so task completion records it for the next turn without sampling again.
        if turn_state.pending_input.items.iter().any(|input| {
            !matches!(
                input,
                TurnInput::InterAgentCommunication(communication) if !communication.trigger_turn
            )
        }) {
            return;
        }
        turn_state.set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    }

    pub(crate) async fn accept_mailbox_delivery_for_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
    ) {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await;
        let Some(turn_state) = turn_state else {
            return;
        };
        self.accept_mailbox_delivery_for_turn_state(turn_state.as_ref())
            .await;
    }

    pub(super) async fn accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) {
        turn_state
            .lock()
            .await
            .accept_mailbox_delivery_for_current_turn();
    }

    pub(super) async fn extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        {
            let mut turn_state = turn_state.lock().await;
            if input
                .iter()
                .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
            {
                turn_state.reserve_mailbox_batch();
            }
            turn_state.pending_input.items.extend(input);
            turn_state.accept_mailbox_delivery_for_current_turn();
        }
        self.activity_tx.send_replace(InputQueueActivity::Steer);
    }

    pub(crate) async fn extend_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
        input: Vec<TurnInput>,
    ) {
        let mut turn_state = turn_state.lock().await;
        if input
            .iter()
            .any(|item| matches!(item, TurnInput::InterAgentCommunication(_)))
        {
            turn_state.reserve_mailbox_batch();
        }
        turn_state.pending_input.items.extend(input);
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        turn_state.lock().await.pending_input.items.split_off(0)
    }

    #[cfg(test)]
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let accepts_mailbox_delivery = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => active_turn
                    .turn_state
                    .lock()
                    .await
                    .accepts_mailbox_delivery_for_current_turn(),
                None => true,
            }
        };
        if !accepts_mailbox_delivery {
            return (Vec::new(), TurnStartOptions::default());
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        self.get_pending_input_with_mailbox(active_turn, mailbox_items, start_options)
            .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input_with_mailbox(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        mailbox_items: Vec<TurnInput>,
        start_options: TurnStartOptions,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (pending_input, accepts_mailbox_delivery, active_turn_metadata) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let active_turn_metadata = active_turn
                        .task
                        .as_ref()
                        .map(|task| Arc::clone(&task.turn_context.turn_metadata_state));
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    if accepts_mailbox_delivery && !mailbox_items.is_empty() {
                        turn_state.reserve_mailbox_batch();
                    }
                    let pending_input = if accepts_mailbox_delivery {
                        turn_state.pending_input.items.split_off(0)
                    } else {
                        Vec::new()
                    };
                    (
                        pending_input,
                        accepts_mailbox_delivery,
                        active_turn_metadata,
                    )
                }
                None => (Vec::new(), true, None),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, TurnStartOptions::default());
        }
        if let Some(active_turn_metadata) = active_turn_metadata
            && mailbox_items.iter().any(|item| {
                matches!(
                    item,
                    TurnInput::InterAgentCommunication(communication)
                        if communication.trigger_turn
                )
            })
            && (start_options.root_turn_id.is_none()
                || active_turn_metadata.root_turn_id() != start_options.root_turn_id)
        {
            active_turn_metadata.mark_root_turn_ambiguous();
        }
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
        }
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery, mailbox_batch_is_reserved) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input.items.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                        turn_state.mailbox_batch_is_reserved(),
                    )
                }
                None => (false, true, false),
            }
        };
        if !accepts_mailbox_delivery {
            return false;
        }
        if has_turn_pending_input {
            return true;
        }
        if mailbox_batch_is_reserved {
            return false;
        }
        self.has_deliverable_mailbox_items().await
    }
}

impl TurnInputQueue {
    fn has_pending_input(&self) -> bool {
        self.items.iter().any(|input| {
            matches!(
                input,
                TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata() {
        let item = ResponseItem::Other;
        let input = TurnInput::ResponseItem(item.clone().into());
        let value = serde_json::json!({"ResponseItem": item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = TurnInput::ResponseItem(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            "ResponseItem": {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let TurnInput::ResponseItem(envelope) = serde_json::from_value(forged).unwrap() else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());
    }

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    #[tokio::test]
    async fn input_queue_notifies_mailbox_subscribers() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending_activity, None);

        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_one, Default::default())
            .await
            .expect("bounded mail");
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two, Default::default())
            .await
            .expect("bounded mail");

        activity_rx.changed().await.expect("mailbox update");
        assert_eq!(
            *activity_rx.borrow_and_update(),
            InputQueueActivity::Mailbox
        );
    }

    #[tokio::test]
    async fn input_queue_notifies_steer_subscribers() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending_activity, None);

        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "steer".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        activity_rx.changed().await.expect("steer update");
        assert_eq!(*activity_rx.borrow_and_update(), InputQueueActivity::Steer);
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    content: vec![UserInput::Text {
                        text: "already pending".to_string(),
                        text_elements: Vec::new(),
                    }],
                    client_id: None,
                }],
            )
            .await;

        let (_activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;

        assert_eq!(pending_activity, Some(InputQueueActivity::Steer));
    }

    #[tokio::test]
    async fn input_queue_drains_mailbox_in_delivery_order() {
        let input_queue = InputQueue::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ true,
        );

        input_queue
            .enqueue_mailbox_communication(mail_one.clone(), Default::default())
            .await
            .expect("bounded mail");
        input_queue
            .enqueue_mailbox_communication(mail_two.clone(), Default::default())
            .await
            .expect("bounded mail");

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one),
                TurnInput::InterAgentCommunication(mail_two)
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_bounds_each_turns_mailbox_batch_without_reordering() {
        let input_queue = InputQueue::new();
        let mails = (0..MAX_MAILBOX_ITEMS_PER_TURN + 1)
            .map(|index| {
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    &format!("mail-{index}"),
                    /*trigger_turn*/ true,
                )
            })
            .collect::<Vec<_>>();
        for mail in &mails {
            input_queue
                .enqueue_mailbox_communication(mail.clone(), Default::default())
                .await
                .expect("bounded mail");
        }

        let first_batch = input_queue.drain_mailbox_input_items().await.0;
        let second_batch = input_queue.drain_mailbox_input_items().await.0;
        let expected_first_batch = mails[..MAX_MAILBOX_ITEMS_PER_TURN]
            .iter()
            .cloned()
            .map(TurnInput::InterAgentCommunication)
            .collect::<Vec<_>>();
        let expected_second_batch = vec![TurnInput::InterAgentCommunication(
            mails[MAX_MAILBOX_ITEMS_PER_TURN].clone(),
        )];

        assert_eq!(first_batch, expected_first_batch);
        assert_eq!(second_batch, expected_second_batch);
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_defers_remaining_mailbox_batches_to_a_fresh_turn() {
        let input_queue = InputQueue::new();
        let mails = (0..MAX_MAILBOX_ITEMS_PER_TURN + 1)
            .map(|index| {
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    &format!("mail-{index}"),
                    /*trigger_turn*/ true,
                )
            })
            .collect::<Vec<_>>();
        for mail in &mails {
            input_queue
                .enqueue_mailbox_communication(mail.clone(), Default::default())
                .await
                .expect("bounded mail");
        }
        let active_turn = Mutex::new(Some(ActiveTurn::default()));
        let first_batch = input_queue.drain_mailbox_input_items().await;

        assert_eq!(
            input_queue
                .get_pending_input_with_mailbox(&active_turn, first_batch.0, first_batch.1)
                .await
                .0,
            mails[..MAX_MAILBOX_ITEMS_PER_TURN]
                .iter()
                .cloned()
                .map(TurnInput::InterAgentCommunication)
                .collect::<Vec<_>>()
        );
        assert!(input_queue.has_pending_mailbox_items().await);
        assert!(!input_queue.has_pending_input(&active_turn).await);

        *active_turn.lock().await = None;
        assert!(input_queue.has_pending_input(&active_turn).await);
        assert_eq!(
            input_queue.get_pending_input(&active_turn).await.0,
            vec![TurnInput::InterAgentCommunication(
                mails[MAX_MAILBOX_ITEMS_PER_TURN].clone()
            )]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_bounds_each_turns_aggregate_mailbox_tokens() {
        let input_queue = InputQueue::new();
        let mails = (0..MAX_MAILBOX_ITEMS_PER_TURN)
            .map(|index| {
                make_mail(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    &format!("mail-{index} {}", "large ".repeat(5_000)),
                    /*trigger_turn*/ true,
                )
            })
            .collect::<Vec<_>>();
        let per_item_tokens = mails
            .iter()
            .map(mailbox_model_token_estimate)
            .collect::<Vec<_>>();
        assert!(
            per_item_tokens
                .iter()
                .all(|tokens| *tokens <= MAX_MAILBOX_MODEL_TOKENS_PER_ITEM)
        );
        assert!(per_item_tokens.iter().sum::<usize>() > MAX_MAILBOX_MODEL_TOKENS_PER_TURN);
        for mail in &mails {
            input_queue
                .enqueue_mailbox_communication(mail.clone(), Default::default())
                .await
                .expect("bounded mail");
        }

        let first_batch = input_queue.drain_mailbox_input_items().await.0;
        assert!(!first_batch.is_empty());
        assert!(first_batch.len() < mails.len());
        assert!(
            first_batch
                .iter()
                .map(|item| match item {
                    TurnInput::InterAgentCommunication(communication) => {
                        mailbox_model_token_estimate(communication)
                    }
                    _ => 0,
                })
                .sum::<usize>()
                <= MAX_MAILBOX_MODEL_TOKENS_PER_TURN
        );
        let mut remaining_count = 0;
        while input_queue.has_pending_mailbox_items().await {
            let batch = input_queue.drain_mailbox_input_items().await.0;
            assert!(!batch.is_empty());
            remaining_count += batch.len();
        }
        assert_eq!(first_batch.len() + remaining_count, mails.len());
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_rejects_oversized_encrypted_mail() {
        let input_queue = InputQueue::new();
        let encrypted_task = InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            format!("delegated task{}", "x".repeat(100_000)),
            /*trigger_turn*/ true,
        );
        assert!(mailbox_model_token_estimate(&encrypted_task) > MAX_MAILBOX_MODEL_TOKENS_PER_ITEM);
        input_queue
            .enqueue_mailbox_communication(encrypted_task, Default::default())
            .await
            .expect_err("oversized encrypted mail must be rejected");

        assert_eq!(input_queue.drain_mailbox_input_items().await.0, Vec::new());
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn mailbox_item_limit_includes_the_final_turn_and_time_stamp() {
        let boundary_mail = (2_500..4_500)
            .find_map(|words| {
                let communication = InterAgentCommunication::new_encrypted(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    Vec::new(),
                    "x ".repeat(words),
                    /*trigger_turn*/ true,
                );
                let unstamped_tokens = usize::try_from(estimate_item_token_count(
                    &mailbox_model_communication(&communication).to_model_input_item(),
                ))
                .unwrap_or(usize::MAX);
                let stamped_tokens = mailbox_model_token_estimate(&communication);
                (unstamped_tokens <= MAX_MAILBOX_MODEL_TOKENS_PER_ITEM
                    && stamped_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_ITEM)
                    .then_some(communication)
            })
            .expect("fixture should cross the item limit only after metadata stamping");

        InputQueue::new()
            .enqueue_mailbox_communication(boundary_mail, Default::default())
            .await
            .expect_err("the final stamped model item must enforce the item limit");
    }

    #[tokio::test]
    async fn legacy_recovery_accepts_preexisting_mail_within_the_turn_bound() {
        let mut communication = InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            "x ".repeat(5_000),
            /*trigger_turn*/ true,
        );
        let id = ResponseItemId::with_suffix("msg", "legacy-large");
        communication.id = Some(id.clone());
        let model_tokens = mailbox_model_token_estimate(&communication);
        assert!(model_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_ITEM);
        assert!(model_tokens <= MAX_MAILBOX_MODEL_TOKENS_PER_TURN);
        assert!(
            prepare_mailbox_model_communication(
                &communication,
                "legacy-turn",
                MailboxModelSizeLimit::Current,
            )
            .is_err()
        );

        let input_queue = InputQueue::new();
        assert!(
            input_queue
                .restore_claimed_mailbox_communication(
                    communication.clone(),
                    Default::default(),
                    "legacy-fingerprint".to_string(),
                    "legacy-turn".to_string(),
                    MailboxModelSizeLimit::LegacyRecovery,
                )
                .await
                .expect("legacy recovery should honor the aggregate turn bound")
        );
        let snapshot = input_queue
            .pending_mailbox_claim_snapshot("legacy-turn")
            .await
            .expect("legacy mail should remain claimable");
        assert_eq!(snapshot.selected_count, 1);
        assert_eq!(snapshot.durable_mail[0].communication_id, id);
    }

    #[tokio::test]
    async fn legacy_recovery_delivers_one_item_when_stamping_crosses_the_turn_bound() {
        let first_words_over_limit = (8_000..40_000)
            .step_by(256)
            .find(|words| {
                let mut communication = InterAgentCommunication::new_encrypted(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    Vec::new(),
                    "x ".repeat(*words),
                    /*trigger_turn*/ true,
                );
                communication.id = Some(ResponseItemId::with_suffix("msg", "legacy-boundary"));
                stamped_mailbox_model_communication(communication, "legacy-turn").model_tokens
                    > MAX_MAILBOX_MODEL_TOKENS_PER_TURN
            })
            .expect("fixture search should cross the normal turn limit");
        let (communication, unstamped_tokens, stamped_tokens) =
            (first_words_over_limit.saturating_sub(256)..=first_words_over_limit)
                .find_map(|words| {
                    let mut communication = InterAgentCommunication::new_encrypted(
                        AgentPath::root(),
                        AgentPath::try_from("/root/worker").expect("agent path"),
                        Vec::new(),
                        "x ".repeat(words),
                        /*trigger_turn*/ true,
                    );
                    communication.id = Some(ResponseItemId::with_suffix("msg", "legacy-boundary"));
                    let unstamped_tokens = usize::try_from(estimate_item_token_count(
                        &communication.to_model_input_item(),
                    ))
                    .unwrap_or(usize::MAX);
                    let stamped_tokens =
                        stamped_mailbox_model_communication(communication.clone(), "legacy-turn")
                            .model_tokens;
                    (unstamped_tokens <= MAX_MAILBOX_MODEL_TOKENS_PER_TURN
                        && stamped_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_TURN
                        && stamped_tokens <= MAX_LEGACY_MAILBOX_MODEL_TOKENS_PER_ITEM)
                        .then_some((communication, unstamped_tokens, stamped_tokens))
                })
                .expect("fixture should cross the turn limit only after metadata stamping");
        let input_queue = InputQueue::new();

        assert!(
            input_queue
                .restore_claimed_mailbox_communication(
                    communication,
                    Default::default(),
                    "legacy-fingerprint".to_string(),
                    "legacy-turn".to_string(),
                    MailboxModelSizeLimit::LegacyRecovery,
                )
                .await
                .expect("legacy item remains below the recovery item limit")
        );
        let snapshot = input_queue
            .pending_mailbox_claim_snapshot("replacement-turn")
            .await
            .expect("one legacy item above the normal turn limit must remain claimable");

        assert!(unstamped_tokens <= MAX_MAILBOX_MODEL_TOKENS_PER_TURN);
        assert!(stamped_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_TURN);
        assert_eq!(snapshot.selected_count, 1);
        assert!(snapshot.durable_mail[0].model_tokens > MAX_MAILBOX_MODEL_TOKENS_PER_TURN);
    }

    #[tokio::test]
    async fn legacy_recovery_preserves_plaintext_beyond_the_current_truncation_limit() {
        let mut communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            &"legacy plaintext ".repeat(1_500),
            /*trigger_turn*/ true,
        );
        communication.id = Some(ResponseItemId::with_suffix("msg", "legacy-plaintext"));
        let input_queue = InputQueue::new();

        assert!(
            input_queue
                .restore_claimed_mailbox_communication(
                    communication.clone(),
                    Default::default(),
                    "legacy-fingerprint".to_string(),
                    "legacy-turn".to_string(),
                    MailboxModelSizeLimit::LegacyRecovery,
                )
                .await
                .expect("valid legacy plaintext should restore")
        );
        let snapshot = input_queue
            .pending_mailbox_claim_snapshot("replacement-turn")
            .await
            .expect("restored legacy plaintext should remain claimable");

        assert_eq!(
            snapshot.durable_mail[0].model_communication.content,
            communication.content
        );
    }

    #[tokio::test]
    async fn mailbox_admission_estimates_the_bounded_agent_paths() {
        let input_queue = InputQueue::new();
        let long_path = AgentPath::try_from(format!("/root/{}", "worker".repeat(8_000)))
            .expect("long agent path remains structurally valid");
        let communication = make_mail(
            long_path,
            AgentPath::root(),
            "bounded path",
            /*trigger_turn*/ true,
        );
        assert!(
            usize::try_from(estimate_item_token_count(
                &communication.to_model_input_item()
            ))
            .unwrap_or(usize::MAX)
                > MAX_MAILBOX_MODEL_TOKENS_PER_TURN
        );
        assert!(mailbox_model_token_estimate(&communication) <= MAX_MAILBOX_MODEL_TOKENS_PER_ITEM);

        input_queue
            .enqueue_mailbox_communication(communication.clone(), Default::default())
            .await
            .expect("bounded model form should be admitted");
        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![TurnInput::InterAgentCommunication(communication)]
        );
    }

    #[tokio::test]
    async fn durable_claim_does_not_absorb_mail_arriving_during_flush() {
        let input_queue = InputQueue::new();
        let first_id = ResponseItemId::with_suffix("msg", "first");
        let mut first = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "first",
            /*trigger_turn*/ true,
        );
        first.id = Some(first_id.clone());
        assert!(
            input_queue
                .prepare_persisted_mailbox_communication(
                    first.clone(),
                    prepare_mailbox_model_communication(
                        &first,
                        "first-turn",
                        MailboxModelSizeLimit::Current,
                    )
                    .expect("valid first model item"),
                    Default::default(),
                    "first-fingerprint".to_string(),
                    "first-turn".to_string(),
                    MailboxModelSizeLimit::Current,
                )
                .await
                .expect("valid first mail")
        );
        assert!(
            input_queue
                .mark_mailbox_communication_persisted(&first_id)
                .await
        );
        let snapshot = input_queue
            .pending_mailbox_claim_snapshot("claimed-turn")
            .await
            .expect("first mail should fit the claimed turn");

        let second_id = ResponseItemId::with_suffix("msg", "second");
        let mut second = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "second",
            /*trigger_turn*/ true,
        );
        second.id = Some(second_id.clone());
        assert!(
            input_queue
                .prepare_persisted_mailbox_communication(
                    second.clone(),
                    prepare_mailbox_model_communication(
                        &second,
                        "second-turn",
                        MailboxModelSizeLimit::Current,
                    )
                    .expect("valid second model item"),
                    Default::default(),
                    "second-fingerprint".to_string(),
                    "second-turn".to_string(),
                    MailboxModelSizeLimit::Current,
                )
                .await
                .expect("valid second mail")
        );
        assert!(
            input_queue
                .mark_mailbox_communication_persisted(&second_id)
                .await
        );

        assert_eq!(
            input_queue
                .drain_claimed_mailbox_input_items(&snapshot, "claimed-turn")
                .await
                .expect("the durable snapshot remains claimable")
                .0,
            vec![TurnInput::InterAgentCommunication(first)]
        );
        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![TurnInput::InterAgentCommunication(second)]
        );
    }

    #[tokio::test]
    async fn interrupted_durable_claim_returns_to_the_front_until_delivery_commits() {
        let input_queue = InputQueue::new();
        let id = ResponseItemId::with_suffix("msg", "interrupted");
        let mut communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "retry me",
            /*trigger_turn*/ true,
        );
        communication.id = Some(id.clone());
        assert!(
            input_queue
                .prepare_persisted_mailbox_communication(
                    communication.clone(),
                    prepare_mailbox_model_communication(
                        &communication,
                        "intended-turn",
                        MailboxModelSizeLimit::Current,
                    )
                    .expect("valid model item"),
                    Default::default(),
                    "fingerprint".to_string(),
                    "intended-turn".to_string(),
                    MailboxModelSizeLimit::Current,
                )
                .await
                .expect("valid durable mail")
        );
        assert!(input_queue.mark_mailbox_communication_persisted(&id).await);

        let first_snapshot = input_queue
            .pending_mailbox_claim_snapshot("first-turn")
            .await
            .expect("mail should fit first turn");
        assert_eq!(
            first_snapshot.durable_mail[0]
                .model_communication
                .internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.turn_id.as_deref()),
            Some("first-turn")
        );
        assert_eq!(
            input_queue
                .drain_claimed_mailbox_input_items(&first_snapshot, "first-turn")
                .await
                .expect("mail should be claimable")
                .0,
            vec![TurnInput::InterAgentCommunication(communication.clone())]
        );
        assert!(!input_queue.durable_mail_was_consumed(&id).await);
        assert_eq!(
            input_queue
                .release_inflight_mail_for_turn("first-turn")
                .await,
            1
        );

        let second_snapshot = input_queue
            .pending_mailbox_claim_snapshot("second-turn")
            .await
            .expect("mail should fit second turn");
        assert_eq!(
            second_snapshot.durable_mail[0]
                .model_communication
                .internal_chat_message_metadata_passthrough
                .as_ref()
                .and_then(|metadata| metadata.turn_id.as_deref()),
            Some("second-turn")
        );
        assert_eq!(
            input_queue
                .drain_claimed_mailbox_input_items(&second_snapshot, "second-turn")
                .await
                .expect("released mail should be reclaimable")
                .0,
            vec![TurnInput::InterAgentCommunication(communication)]
        );
        assert!(
            input_queue
                .mark_inflight_mail_delivered(&id, "second-turn")
                .await
        );
        assert!(input_queue.durable_mail_was_consumed(&id).await);
        assert_eq!(
            input_queue
                .release_inflight_mail_for_turn("second-turn")
                .await,
            0
        );
    }

    #[tokio::test]
    async fn indeterminate_delivery_commit_is_not_requeued_or_replayed() {
        let input_queue = InputQueue::new();
        let id = ResponseItemId::with_suffix("msg", "indeterminate");
        let mut communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "do not replay me",
            /*trigger_turn*/ true,
        );
        communication.id = Some(id.clone());
        assert!(
            input_queue
                .prepare_persisted_mailbox_communication(
                    communication.clone(),
                    prepare_mailbox_model_communication(
                        &communication,
                        "commit-turn",
                        MailboxModelSizeLimit::Current,
                    )
                    .expect("valid model item"),
                    Default::default(),
                    "fingerprint".to_string(),
                    "commit-turn".to_string(),
                    MailboxModelSizeLimit::Current,
                )
                .await
                .expect("valid durable mail")
        );
        assert!(input_queue.mark_mailbox_communication_persisted(&id).await);
        let snapshot = input_queue
            .pending_mailbox_claim_snapshot("commit-turn")
            .await
            .expect("mail should fit the committing turn");
        input_queue
            .drain_claimed_mailbox_input_items(&snapshot, "commit-turn")
            .await
            .expect("mail should be claimed");
        assert!(
            input_queue
                .mark_inflight_mail_commit_started(&id, "commit-turn")
                .await
        );

        assert_eq!(
            input_queue
                .release_inflight_mail_for_turn("commit-turn")
                .await,
            0
        );
        assert_eq!(
            input_queue.durable_mail_retry_status(&id).await,
            DurableMailRetryStatus::InFlight
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn pending_turn_id_is_limited_to_the_selected_batch() {
        let input_queue = InputQueue::new();
        for index in 0..MAX_MAILBOX_ITEMS_PER_TURN {
            input_queue
                .enqueue_mailbox_communication(
                    make_mail(
                        AgentPath::root(),
                        AgentPath::try_from("/root/worker").expect("agent path"),
                        &format!("mail-{index}"),
                        /*trigger_turn*/ false,
                    ),
                    Default::default(),
                )
                .await
                .expect("bounded mail");
        }
        let durable_id = ResponseItemId::with_suffix("msg", "next-batch");
        let mut durable = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "next batch",
            /*trigger_turn*/ true,
        );
        durable.id = Some(durable_id.clone());
        assert!(
            input_queue
                .prepare_persisted_mailbox_communication(
                    durable.clone(),
                    prepare_mailbox_model_communication(
                        &durable,
                        "next-turn",
                        MailboxModelSizeLimit::Current,
                    )
                    .expect("valid next model item"),
                    Default::default(),
                    "next-fingerprint".to_string(),
                    "next-turn".to_string(),
                    MailboxModelSizeLimit::Current,
                )
                .await
                .expect("valid durable mail")
        );
        assert!(
            input_queue
                .mark_mailbox_communication_persisted(&durable_id)
                .await
        );

        assert_eq!(input_queue.pending_mailbox_turn_id().await, None);
    }

    #[tokio::test]
    async fn input_queue_requires_one_unambiguous_trigger_parent() {
        let (parent, peer, root, root2) = (Some("a"), Some("b"), Some("r"), Some("s"));
        for (pending_mails, expected_parent_turn_id, expected_root_turn_id) in [
            (Vec::new(), None, None),
            (vec![(false, Some("q"), root)], None, None),
            (vec![(true, Some(""), root)], None, None),
            (vec![(true, Some("   "), root)], None, None),
            (vec![(true, None, root)], None, None),
            (vec![(true, parent, None)], parent, None),
            (vec![(true, parent, Some(""))], parent, None),
            (vec![(true, parent, root), (true, peer, root)], None, root),
            (vec![(true, parent, root), (true, peer, root2)], None, None),
            (vec![(true, parent, root), (true, None, root)], None, None),
            (
                vec![(true, parent, root), (true, parent, root)],
                parent,
                root,
            ),
            (
                vec![(false, Some("q"), root2), (true, parent, root)],
                parent,
                root,
            ),
        ] {
            let input_queue = InputQueue::new();
            for (trigger_turn, parent_turn_id, root_turn_id) in pending_mails {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("bounded mail");
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(
                start_options.parent_turn_id.as_deref(),
                expected_parent_turn_id
            );
            assert_eq!(start_options.root_turn_id.as_deref(), expected_root_turn_id);
        }
    }

    #[tokio::test]
    async fn input_queue_uses_latest_followup_choice_and_ignores_queue_only_mail() {
        use codex_protocol::turn_input::CyberAccessProgram;

        for latest in [Some(CyberAccessProgram::Standard), None] {
            let input_queue = InputQueue::new();
            for (trigger_turn, program) in [
                (true, Some(CyberAccessProgram::DaybreakBlue)),
                (true, latest),
                (false, Some(CyberAccessProgram::DaybreakRed)),
            ] {
                input_queue
                    .enqueue_mailbox_communication(
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("bounded mail");
            }
            let (_, start_options) = input_queue.drain_mailbox_input_items().await;
            assert_eq!(start_options.cyber_access_program, latest);
        }
    }

    #[tokio::test]
    async fn input_queue_tracks_pending_trigger_turn_mail() {
        let input_queue = InputQueue::new();

        let queued_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(queued_mail, Default::default())
            .await
            .expect("bounded mail");
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail, Default::default())
            .await
            .expect("bounded mail");
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}
