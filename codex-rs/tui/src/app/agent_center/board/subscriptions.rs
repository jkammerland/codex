//! Subscription intent remains pending until its accepted revision is visible.

use super::*;

pub(crate) struct PendingSubscription {
    pub(crate) target: TeamBoardSubscriptionTarget,
    pub(crate) identity: TeamBoardIdentity,
    pub(crate) mode: TeamBoardSubscriptionMode,
    pub(crate) accepted_revision: Option<i64>,
    pub(crate) failed: bool,
}

pub(crate) fn mode_label(mode: TeamBoardSubscriptionMode) -> &'static str {
    match mode {
        TeamBoardSubscriptionMode::Off => "Off",
        TeamBoardSubscriptionMode::Watch => "Watch",
        TeamBoardSubscriptionMode::Wake => "Wake",
    }
}

impl BoardState {
    pub(crate) fn subscription_mode(
        &self,
        target: &TeamBoardSubscriptionTarget,
        identity: &TeamBoardIdentity,
    ) -> TeamBoardSubscriptionMode {
        match target {
            TeamBoardSubscriptionTarget::Conversation { conversation_id } => self
                .snapshot
                .followers
                .iter()
                .find(|follow| {
                    follow.conversation_id == *conversation_id && follow.identity == *identity
                })
                .map_or(TeamBoardSubscriptionMode::Off, |follow| follow.mode),
            TeamBoardSubscriptionTarget::Topic { topic } => self
                .snapshot
                .topic_subscriptions
                .iter()
                .find(|follow| follow.topic == *topic && follow.identity == *identity)
                .map_or(TeamBoardSubscriptionMode::Off, |follow| follow.mode),
        }
    }

    pub(crate) fn subscription_status(
        &self,
        target: &TeamBoardSubscriptionTarget,
        identity: &TeamBoardIdentity,
    ) -> String {
        let confirmed = mode_label(self.subscription_mode(target, identity));
        let pending = self
            .pending_subscriptions
            .values()
            .find(|pending| pending.target == *target && pending.identity == *identity);
        match pending {
            Some(pending) => format!(
                "{confirmed} · {} {}",
                mode_label(pending.mode),
                if pending.failed {
                    "unconfirmed"
                } else if pending.accepted_revision.is_some() {
                    "syncing"
                } else {
                    "pending"
                }
            ),
            None => confirmed.into(),
        }
    }

    pub(crate) fn begin_subscription(
        &mut self,
        target: TeamBoardSubscriptionTarget,
        identity: TeamBoardIdentity,
        mode: TeamBoardSubscriptionMode,
    ) -> Option<TeamOperation> {
        if self
            .pending_subscriptions
            .values()
            .any(|pending| pending.target == target && pending.identity == identity)
        {
            self.notice = Some(
                "This subscription has a pending change · Ctrl+R retries an unknown outcome".into(),
            );
            return None;
        }
        if self.subscription_mode(&target, &identity) == mode {
            return None;
        }
        let operation = TeamOperation::new(TeamRequest::Board {
            request: TeamBoardParams::SubscriptionSet {
                target: target.clone(),
                identity: Some(identity.clone()),
                mode,
            },
        });
        self.pending_subscriptions.insert(
            operation.operation_id.clone(),
            PendingSubscription {
                target,
                identity,
                mode,
                accepted_revision: None,
                failed: false,
            },
        );
        self.notice = None;
        Some(operation)
    }

    pub(crate) fn subscription_accepted(&mut self, operation_id: &str, revision: i64) {
        if let Some(pending) = self.pending_subscriptions.get_mut(operation_id) {
            pending.accepted_revision = Some(revision);
            pending.failed = false;
        }
        self.confirm_subscriptions();
    }

    pub(super) fn confirm_subscriptions(&mut self) {
        // A newer accepted mutation may supersede ours; display the observed mode.
        let before = self.pending_subscriptions.len();
        self.pending_subscriptions.retain(|_, pending| {
            pending
                .accepted_revision
                .is_none_or(|revision| self.snapshot.revision < revision)
        });
        if before != self.pending_subscriptions.len() {
            self.notice = Some(
                "Subscription synchronized · accepted deliveries keep their recipients".into(),
            );
        }
    }
}
