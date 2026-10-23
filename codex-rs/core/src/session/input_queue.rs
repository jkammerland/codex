use crate::agent::api::AgentControl;
use crate::agent_communication::PENDING_MAILBOX_MESSAGES;
use crate::state::ActiveTurn;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;
use codex_diagnostics::GaugeGuard;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use indexmap::IndexMap;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

/// Host capture metadata belonging to one input, including steers within another turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_order: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "codex_history::UserInputOrigin::is_user"
    )]
    pub origin: codex_history::UserInputOrigin,
}

/// Input consumed by a regular turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
        #[serde(flatten)]
        metadata: UserInputMetadata,
    },
    FunctionCallOutput(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    // Preserve the existing serialized format while carrying injection API metadata
    // through the in-memory queue.
    ResponseItem(#[serde(with = "turn_input_response_item")] ResponseItemEnvelope),
    InterAgentCommunication(crate::PendingAgentMessage),
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
    items: Vec<TurnInput>,
    /// Response items queued through a notifying injection, unlike passive context.
    has_pending_injection: bool,
}

#[derive(Default)]
struct MailboxState {
    pending: VecDeque<PendingMailboxCommunication>,
    recorded: IndexMap<String, InterAgentCommunication>,
    submitted: HashSet<String>,
}

impl MailboxState {
    /// Return rejected mail so its attachments can be dropped after unlocking.
    fn enqueue(
        &mut self,
        communication: crate::PendingAgentMessage,
        start_options: TurnStartOptions,
    ) -> Option<crate::PendingAgentMessage> {
        if communication.mail.as_ref().is_some_and(|metadata| {
            self.submitted.contains(&metadata.message_id)
                || self.pending.iter().any(|mail| {
                    mail.communication
                        .mail
                        .as_ref()
                        .is_some_and(|existing| existing.message_id == metadata.message_id)
                })
        }) {
            return Some(communication);
        }
        self.pending.push_back(PendingMailboxCommunication {
            communication,
            start_options,
            _diagnostics_guard: PENDING_MAILBOX_MESSAGES.track(),
        });
        None
    }
}

/// Session-scoped pending input storage and active-turn mailbox delivery coordination.
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,
    controller: Option<(ThreadId, Arc<dyn AgentControl>, watch::Receiver<bool>)>,
    // Recording history is not a transport receipt. Retain wake debt until request acceptance.
    // Queue admission shares the receipt lock so delayed delivery cannot revive submitted mail.
    mailbox: Mutex<MailboxState>,
}

pub(crate) struct PendingMailboxCommunication {
    pub(crate) communication: crate::PendingAgentMessage,
    start_options: TurnStartOptions,
    _diagnostics_guard: GaugeGuard,
}

impl InputQueue {
    pub(crate) fn new() -> Self {
        let (activity_tx, _) = watch::channel(InputQueueActivity::Mailbox);
        Self {
            activity_tx,
            controller: None,
            mailbox: Mutex::new(MailboxState::default()),
        }
    }

    pub(crate) fn with_controller(thread_id: ThreadId, control: Arc<dyn AgentControl>) -> Self {
        let updates = control.watch_mailbox(thread_id);
        Self {
            controller: Some((thread_id, control, updates)),
            ..Self::new()
        }
    }

    pub(crate) fn mailbox_updates(&self) -> Option<watch::Receiver<bool>> {
        self.controller
            .as_ref()
            .map(|(_, _, updates)| updates.clone())
    }

    pub(crate) fn notify_mailbox(&self) {
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
    }

    fn read_mailbox(
        &self,
        mailbox: &mut MailboxState,
        rejected: &mut Vec<crate::PendingAgentMessage>,
    ) {
        if let Some((thread_id, control, _)) = &self.controller {
            for communication in control.take_mailbox(*thread_id) {
                if let Some(communication) =
                    mailbox.enqueue(communication, TurnStartOptions::default())
                {
                    rejected.push(communication);
                }
            }
        }
    }

    pub(crate) async fn subscribe_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
    ) -> (
        watch::Receiver<InputQueueActivity>,
        Option<InputQueueActivity>,
    ) {
        let activity_rx = self.activity_tx.subscribe();
        let turn_activity = if let Some(turn_state) = turn_state {
            turn_state.lock().await.pending_input.pending_activity()
        } else {
            None
        };
        let pending_activity = if let Some(activity) = turn_activity {
            Some(activity)
        } else if self.has_pending_mailbox_items().await {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        };
        (activity_rx, pending_activity)
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn deliver_mailbox_communication_to_current_turn(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        communication: impl Into<crate::PendingAgentMessage>,
        expected_turn_id: Option<&str>,
    ) -> bool {
        let communication = communication.into();
        let active = active_turn.lock().await;
        let Some(active_turn) = active.as_ref().filter(|turn| {
            turn.task.as_ref().is_some_and(|task| {
                expected_turn_id.is_none_or(|id| {
                    task.turn_context.sub_id == id && !task.cancellation_token.is_cancelled()
                })
            })
        }) else {
            return false;
        };
        let mut turn_state = active_turn.turn_state.lock().await;
        if !turn_state.accepts_mailbox_delivery_for_current_turn() {
            return false;
        }
        turn_state
            .pending_input
            .items
            .push(TurnInput::InterAgentCommunication(communication));
        self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        true
    }

    /// Returns whether this delivery added a new pending message.
    pub(crate) async fn enqueue_mailbox_communication(
        &self,
        communication: crate::PendingAgentMessage,
        start_options: TurnStartOptions,
    ) -> bool {
        // Declared before the guard so rejected attachments drop after unlocking.
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        // Mail retained while unloaded precedes new submissions to the loaded session.
        self.read_mailbox(&mut mailbox, &mut rejected);
        let inserted = match mailbox.enqueue(communication, start_options) {
            None => true,
            Some(communication) => {
                rejected.push(communication);
                false
            }
        };
        if inserted {
            self.activity_tx.send_replace(InputQueueActivity::Mailbox);
        }
        inserted
    }

    pub(crate) async fn has_pending_mailbox_items(&self) -> bool {
        let mailbox = self.mailbox.lock().await;
        !mailbox.pending.is_empty()
            || !mailbox.recorded.is_empty()
            || self
                .controller
                .as_ref()
                .is_some_and(|(_, _, updates)| *updates.borrow())
    }

    pub(crate) async fn has_wakeable_mailbox_items(&self) -> bool {
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        self.read_mailbox(&mut mailbox, &mut rejected);
        mailbox
            .pending
            .iter()
            .any(|mail| mail_wakes(&mail.communication))
            || mailbox.recorded.values().any(mail_wakes)
    }

    /// A failed request retains its receipt debt, but that debt cannot retry the
    /// request itself. A newly queued message can authorize another continuation.
    pub(crate) async fn has_new_wakeable_mailbox_items(&self) -> bool {
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        self.read_mailbox(&mut mailbox, &mut rejected);
        mailbox.pending.iter().any(|mail| {
            mail_wakes(&mail.communication)
                && mail
                    .communication
                    .mail
                    .as_ref()
                    .is_none_or(|metadata| !mailbox.recorded.contains_key(&metadata.message_id))
        })
    }

    pub(crate) async fn record_mail(&self, communication: &InterAgentCommunication) {
        if let Some(metadata) = &communication.mail {
            let mut receipts = self.mailbox.lock().await;
            if !receipts.submitted.contains(&metadata.message_id) {
                receipts
                    .recorded
                    .insert(metadata.message_id.clone(), communication.clone());
            }
        }
    }

    pub(crate) async fn discard_settled_assignments(&self, task_ids: &HashSet<String>) {
        let retain = |mail: &InterAgentCommunication| {
            !mail.mail.as_ref().is_some_and(|metadata| {
                metadata.kind == codex_protocol::protocol::AgentMailKind::Assignment
                    && metadata
                        .task_id
                        .as_ref()
                        .is_some_and(|id| task_ids.contains(id))
            })
        };
        let mut mailbox = self.mailbox.lock().await;
        mailbox.recorded.retain(|_, mail| retain(mail));
        let (retained, removed) = std::mem::take(&mut mailbox.pending)
            .into_iter()
            .partition::<VecDeque<_>, _>(|mail| retain(&mail.communication));
        mailbox.pending = retained;
        drop(mailbox);
        drop(removed);
    }

    pub(crate) async fn unsubmitted_recorded_mail(&self) -> Vec<InterAgentCommunication> {
        self.mailbox
            .lock()
            .await
            .recorded
            .values()
            .cloned()
            .collect()
    }

    pub(crate) async fn was_mail_submitted(&self, id: &str) -> bool {
        self.mailbox.lock().await.submitted.contains(id)
    }

    pub(crate) async fn accept_submitted_mail(&self, items: &[ResponseItem]) {
        let mut receipts = self.mailbox.lock().await;
        for item in items {
            let id = match item {
                ResponseItem::AgentMessage { id, .. } | ResponseItem::Message { id, .. } => {
                    id.as_ref()
                }
                _ => None,
            };
            if let Some(id) = id
                && receipts.recorded.shift_remove(id.as_str()).is_some()
            {
                receipts.submitted.insert(id.to_string());
            }
        }
        // Reconciliation may submit a message before its original delivery Op
        // reaches this queue. Remove copies that arrived before the receipt too.
        let MailboxState {
            pending, submitted, ..
        } = &mut *receipts;
        let (retained, removed) = std::mem::take(pending)
            .into_iter()
            .partition::<VecDeque<_>, _>(|mail| {
                mail.communication
                    .mail
                    .as_ref()
                    .is_none_or(|metadata| !submitted.contains(&metadata.message_id))
            });
        *pending = retained;
        drop(receipts);
        drop(removed);
    }

    /// Selected completion records satisfy the barrier through the durable task store.
    /// Other waking input interrupts the barrier without consuming its mailbox entry.
    pub(crate) async fn pending_barrier_activity(
        &self,
        turn_state: Option<&Mutex<TurnState>>,
        selected_task_ids: &[String],
    ) -> Option<InputQueueActivity> {
        let interrupts = |mail: &InterAgentCommunication| {
            mail_wakes(mail)
                && !mail.mail.as_ref().is_some_and(|metadata| {
                    matches!(
                        metadata.kind,
                        codex_protocol::protocol::AgentMailKind::Completion
                            | codex_protocol::protocol::AgentMailKind::LegacyCompletion
                    ) && metadata
                        .task_id
                        .as_ref()
                        .is_some_and(|id| selected_task_ids.contains(id))
                })
        };
        if let Some(turn_state) = turn_state {
            let state = turn_state.lock().await;
            if state.pending_input.has_pending_steering() {
                return Some(InputQueueActivity::Steer);
            }
            if state.pending_input.items.iter().any(
                |item| matches!(item, TurnInput::InterAgentCommunication(mail) if interrupts(mail)),
            ) {
                return Some(InputQueueActivity::Mailbox);
            }
        }
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        self.read_mailbox(&mut mailbox, &mut rejected);
        mailbox
            .pending
            .iter()
            .any(|mail| interrupts(&mail.communication))
            .then_some(InputQueueActivity::Mailbox)
    }

    pub(crate) async fn has_trigger_turn_mailbox_items(&self) -> bool {
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        self.read_mailbox(&mut mailbox, &mut rejected);
        mailbox
            .pending
            .iter()
            .any(|mail| mail.communication.trigger_turn)
    }

    pub(crate) async fn drain_mailbox(&self) -> Vec<PendingMailboxCommunication> {
        let mut rejected = Vec::new();
        let mut mailbox = self.mailbox.lock().await;
        self.read_mailbox(&mut mailbox, &mut rejected);
        mailbox.pending.drain(..).collect()
    }

    pub(crate) async fn drain_mailbox_input_items(&self) -> (Vec<TurnInput>, TurnStartOptions) {
        let pending_mails = self.drain_mailbox().await;
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
            .find(|mail| mail.communication.trigger_turn)
            .and_then(|mail| {
                mail.start_options
                    .parent_turn_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .and(mail.start_options.root_turn_id.as_deref())
                    .filter(|id| !id.trim().is_empty())
            })
            .map(str::to_string);
        let items = pending_mails
            .into_iter()
            .map(|mail| TurnInput::InterAgentCommunication(mail.communication))
            .collect();
        (items, start_options)
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

    /// Signal once a user message is queued for this sampling request.
    pub(crate) async fn watch_user_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
        sub_id: &str,
        interrupt: CancellationToken,
    ) -> Option<AbortOnDropHandle<()>> {
        let turn_state = self.turn_state_for_sub_id(active_turn, sub_id).await?;
        // Subscribe before inspecting the queue so an arrival cannot be missed.
        let mut activity = self.activity_tx.subscribe();
        Some(AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                if turn_state.lock().await.pending_input.has_user_input() {
                    interrupt.cancel();
                    return;
                }
                if activity.changed().await.is_err() {
                    return;
                }
            }
        })))
    }

    /// Clear any pending waiters and input buffered for the current turn.
    pub(crate) async fn clear_pending(&self, active_turn: &ActiveTurn) {
        let pending = {
            let mut turn_state = active_turn.turn_state.lock().await;
            turn_state.clear_pending_waiters();
            // Reset injection state, retaining mail for the next admitted turn.
            std::mem::take(&mut turn_state.pending_input)
        };
        // Extension attachments may acquire queue locks when dropped.
        for input in pending.items {
            if let TurnInput::InterAgentCommunication(communication) = input {
                self.enqueue_mailbox_communication(communication, TurnStartOptions::default())
                    .await;
            }
        }
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
            turn_state.pending_input.has_pending_injection |= input
                .iter()
                .any(|input| matches!(input, TurnInput::ResponseItem(_)));
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
        turn_state.lock().await.pending_input.items.extend(input);
    }

    pub(crate) async fn take_pending_input_for_turn_state(
        &self,
        turn_state: &Mutex<TurnState>,
    ) -> Vec<TurnInput> {
        std::mem::take(&mut turn_state.lock().await.pending_input).items
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub(crate) async fn get_pending_input(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> (Vec<TurnInput>, TurnStartOptions) {
        let (pending_input, accepts_mailbox_delivery) = {
            let mut active = active_turn.lock().await;
            match active.as_mut() {
                Some(active_turn) => {
                    let mut turn_state = active_turn.turn_state.lock().await;
                    let accepts_mailbox_delivery =
                        turn_state.accepts_mailbox_delivery_for_current_turn();
                    let pending_input = if accepts_mailbox_delivery {
                        std::mem::take(&mut turn_state.pending_input).items
                    } else {
                        Vec::new()
                    };
                    (pending_input, accepts_mailbox_delivery)
                }
                None => (Vec::new(), true),
            }
        };
        if !accepts_mailbox_delivery {
            return (pending_input, TurnStartOptions::default());
        }
        let (mailbox_items, start_options) = self.drain_mailbox_input_items().await;
        if pending_input.is_empty() {
            (mailbox_items, start_options)
        } else {
            let mut pending_input = pending_input;
            pending_input.extend(mailbox_items);
            (pending_input, start_options)
        }
    }

    /// Close team steering in the same critical section as the final pending check.
    /// Rejected deliveries remain with the host for the next eligible turn.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "the final input check must serialize with team steering admission"
    )]
    pub(crate) async fn close_team_input_if_idle(
        &self,
        active_turn: &Mutex<Option<ActiveTurn>>,
    ) -> bool {
        let active = active_turn.lock().await;
        let Some(active_turn) = active.as_ref() else {
            return true;
        };
        let mut state = active_turn.turn_state.lock().await;
        if state.accepts_mailbox_delivery_for_current_turn()
            && (!state.pending_input.is_empty() || self.has_pending_mailbox_items().await)
        {
            return false;
        }
        state.team_input_closed = true;
        true
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state reads must remain atomic"
    )]
    pub(crate) async fn has_pending_input(&self, active_turn: &Mutex<Option<ActiveTurn>>) -> bool {
        let (has_turn_pending_input, accepts_mailbox_delivery) = {
            let active = active_turn.lock().await;
            match active.as_ref() {
                Some(active_turn) => {
                    let turn_state = active_turn.turn_state.lock().await;
                    (
                        !turn_state.pending_input.is_empty(),
                        turn_state.accepts_mailbox_delivery_for_current_turn(),
                    )
                }
                None => (false, true),
            }
        };
        if !accepts_mailbox_delivery {
            return false;
        }
        if has_turn_pending_input {
            return true;
        }
        self.has_pending_mailbox_items().await
    }
}

impl TurnInputQueue {
    fn has_user_input(&self) -> bool {
        self.items
            .iter()
            .any(|input| matches!(input, TurnInput::UserInput { .. }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn has_pending_steering(&self) -> bool {
        self.has_pending_injection
            || self.items.iter().any(|input| {
                matches!(
                    input,
                    TurnInput::UserInput { .. } | TurnInput::FunctionCallOutput(_)
                )
            })
    }

    fn pending_activity(&self) -> Option<InputQueueActivity> {
        if self.has_pending_steering() {
            Some(InputQueueActivity::Steer)
        } else if self
            .items
            .iter()
            .any(|input| matches!(input, TurnInput::InterAgentCommunication(_)))
        {
            Some(InputQueueActivity::Mailbox)
        } else {
            None
        }
    }
}

fn mail_wakes(mail: &InterAgentCommunication) -> bool {
    mail.mail
        .as_ref()
        .is_none_or(|metadata| metadata.wake == codex_protocol::protocol::AgentMailWake::Auto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_protocol::AgentPath;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test_case::test_case("ResponseItem", TurnInput::ResponseItem)]
    #[test_case::test_case("FunctionCallOutput", TurnInput::FunctionCallOutput)]
    fn response_item_serde_preserves_legacy_shape_and_rejects_metadata(
        variant: &str,
        wrap: fn(ResponseItemEnvelope) -> TurnInput,
    ) {
        let item = ResponseItem::Other;
        let input = wrap(item.clone().into());
        let value = serde_json::json!({variant: item});

        assert_eq!(serde_json::to_value(&input).unwrap(), value);
        assert_eq!(serde_json::from_value::<TurnInput>(value).unwrap(), input);

        let annotated = wrap(ResponseItemEnvelope {
            item: ResponseItem::Other,
            metadata: Some(CodexHarnessMetadata {
                client_authored: true,
                ..Default::default()
            }),
        });
        assert!(serde_json::to_value(annotated).is_err());

        let forged = serde_json::json!({
            variant: {
                "type": "message",
                "role": "developer",
                "content": [],
                "metadata": {"client_authored": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged).unwrap()
        else {
            panic!("expected response item");
        };
        assert!(envelope.metadata.is_none());

        let forged_configuration = serde_json::json!({
            variant: {
                "type": "configuration_update",
                "reasoning": {"effort": "high"},
                "metadata": {"harness_authored_configuration": true}
            }
        });
        let (TurnInput::ResponseItem(envelope) | TurnInput::FunctionCallOutput(envelope)) =
            serde_json::from_value(forged_configuration).unwrap()
        else {
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
    async fn submitted_mail_cannot_regain_recorded_debt_or_authorize_a_new_wake() {
        let input_queue = InputQueue::new();
        let make_typed_mail = |id: &str| {
            let mut mail = make_mail(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                id,
                /*trigger_turn*/ false,
            );
            mail.id = Some(codex_protocol::ResponseItemId::from_server(id.to_string()));
            mail.mail = Some(codex_protocol::protocol::AgentMailMetadata {
                message_id: id.to_string(),
                task_id: None,
                kind: codex_protocol::protocol::AgentMailKind::DirectMessage,
                wake: codex_protocol::protocol::AgentMailWake::Auto,
            });
            mail
        };
        let submitted = make_typed_mail("submitted");
        input_queue.record_mail(&submitted).await;
        input_queue
            .enqueue_mailbox_communication(submitted.clone().into(), Default::default())
            .await;
        assert!(input_queue.has_wakeable_mailbox_items().await);

        // History recording alone retains debt; only the exact submitted input
        // clears it, and a delayed duplicate recording cannot recreate it.
        input_queue
            .accept_submitted_mail(&[ResponseItem::Other])
            .await;
        assert!(input_queue.has_wakeable_mailbox_items().await);
        input_queue
            .accept_submitted_mail(&[submitted.to_model_input_item()])
            .await;
        input_queue.record_mail(&submitted).await;
        assert!(input_queue.was_mail_submitted("submitted").await);
        assert!(input_queue.unsubmitted_recorded_mail().await.is_empty());
        assert!(!input_queue.has_wakeable_mailbox_items().await);

        // A stale pending copy is not fresh authorization after receipt. A new
        // message remains eligible even when that stale copy precedes it.
        input_queue
            .enqueue_mailbox_communication(submitted.into(), Default::default())
            .await;
        assert!(!input_queue.has_wakeable_mailbox_items().await);
        assert!(!input_queue.has_new_wakeable_mailbox_items().await);
        assert!(input_queue.drain_mailbox().await.is_empty());
        input_queue
            .enqueue_mailbox_communication(make_typed_mail("new").into(), Default::default())
            .await;
        assert!(input_queue.has_new_wakeable_mailbox_items().await);
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
            .enqueue_mailbox_communication(mail_one.into(), Default::default())
            .await;
        let mail_two = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        );
        input_queue
            .enqueue_mailbox_communication(mail_two.into(), Default::default())
            .await;

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
                    metadata: Default::default(),
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
    async fn clearing_pending_injection_resets_steering_and_retains_mail() {
        let input_queue = InputQueue::new();
        let active = ActiveTurn::default();
        let passive = TurnInput::ResponseItem(ResponseItem::Other.into());
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &active.turn_state,
                vec![passive.clone()],
            )
            .await;
        let mail = make_mail(AgentPath::root(), AgentPath::root(), "retained", false);
        input_queue
            .extend_pending_input_for_turn_state(
                &active.turn_state,
                vec![TurnInput::InterAgentCommunication(mail.clone().into())],
            )
            .await;
        assert_eq!(
            input_queue
                .pending_barrier_activity(Some(&active.turn_state), &[])
                .await,
            Some(InputQueueActivity::Steer)
        );
        input_queue.clear_pending(&active).await;
        input_queue
            .extend_pending_input_for_turn_state(&active.turn_state, vec![passive])
            .await;
        assert_eq!(
            input_queue
                .subscribe_activity(Some(&active.turn_state))
                .await
                .1,
            Some(InputQueueActivity::Mailbox)
        );
        let retained = input_queue.drain_mailbox().await;
        assert_eq!(retained.len(), 1);
        assert_eq!(*retained[0].communication, mail);
        assert_eq!(
            input_queue
                .pending_barrier_activity(Some(&active.turn_state), &[])
                .await,
            None
        );
    }

    #[tokio::test]
    async fn input_queue_reports_already_pending_steer() {
        let input_queue = InputQueue::new();
        let turn_state = Mutex::new(TurnState::default());
        let passive_output: TurnInput = serde_json::from_value(serde_json::json!({
            "ResponseItem": {"type": "function_call_output", "name": "notify", "output": "passive"}
        }))
        .unwrap();
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output.clone()])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![passive_output.clone()],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            Some(InputQueueActivity::Steer)
        );
        // Consuming the injection must not make later passive context wake a wait.
        input_queue
            .take_pending_input_for_turn_state(&turn_state)
            .await;
        input_queue
            .extend_pending_input_for_turn_state(&turn_state, vec![passive_output])
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            None
        );
        let communication = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "already pending mail",
            /*trigger_turn*/ false,
        );
        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![TurnInput::InterAgentCommunication(communication.into())],
            )
            .await;
        assert_eq!(
            input_queue.subscribe_activity(Some(&turn_state)).await.1,
            Some(InputQueueActivity::Mailbox)
        );
        input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                &turn_state,
                vec![TurnInput::UserInput {
                    metadata: Default::default(),
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
            .enqueue_mailbox_communication(mail_one.clone().into(), Default::default())
            .await;
        input_queue
            .enqueue_mailbox_communication(mail_two.clone().into(), Default::default())
            .await;

        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(mail_one.into()),
                TurnInput::InterAgentCommunication(mail_two.into())
            ]
        );
        assert!(!input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn input_queue_uses_unambiguous_trigger_parent_and_first_root() {
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
            (vec![(true, parent, root), (true, peer, root2)], None, root),
            (vec![(true, parent, root), (true, None, root)], None, root),
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
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn)
                            .into(),
                        TurnStartOptions {
                            parent_turn_id: parent_turn_id.map(str::to_string),
                            root_turn_id: root_turn_id.map(str::to_string),
                            ..Default::default()
                        },
                    )
                    .await;
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
                        make_mail(AgentPath::root(), AgentPath::root(), "task", trigger_turn)
                            .into(),
                        TurnStartOptions {
                            cyber_access_program: program,
                            ..Default::default()
                        },
                    )
                    .await;
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
            .enqueue_mailbox_communication(queued_mail.into(), Default::default())
            .await;
        assert!(!input_queue.has_trigger_turn_mailbox_items().await);

        let trigger_mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        );
        input_queue
            .enqueue_mailbox_communication(trigger_mail.into(), Default::default())
            .await;
        assert!(input_queue.has_trigger_turn_mailbox_items().await);
    }
}

#[cfg(test)]
#[path = "input_queue_mail_lifecycle_tests.rs"]
mod mail_lifecycle_tests;
