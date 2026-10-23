//! Retained conversations are another section of the existing agent command center.
//! The coordinator owns identity, order and delivery; this view owns drafts and reading position.

mod input;
mod render;
mod subscriptions;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
mod topics;

use crate::app::agents_overview::AGENTS_OVERVIEW_VIEW_ID;
use crate::app::agents_overview_view::command_center::TeamControlSummary;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ChatComposer;
use crate::bottom_pane::ComposerDraft;
use crate::bottom_pane::ViewCompletion;
use codex_app_server_protocol::TeamBoardEntry;
use codex_app_server_protocol::TeamBoardIdentity;
use codex_app_server_protocol::TeamBoardParams;
use codex_app_server_protocol::TeamBoardSnapshot;
use codex_app_server_protocol::TeamBoardSubscriptionMode;
use codex_app_server_protocol::TeamBoardSubscriptionTarget;
use codex_team::TeamOperation;
use codex_team::TeamRequest;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    #[default]
    All,
    Following,
    Mentions,
}

pub(crate) struct PendingDraft {
    pub(crate) operation_id: String,
    pub(crate) revision: u64,
    pub(crate) failed: bool,
}

pub(crate) struct PendingRead {
    pub(crate) conversation: String,
    pub(crate) append_seq: i64,
    pub(crate) failed: bool,
    pub(crate) confirmed: bool,
}

pub(crate) struct DisplayedEntries {
    pub(crate) conversation: String,
    pub(crate) width: u16,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) entries: Vec<(i64, usize)>,
}

pub(crate) struct ScrollAnchor {
    entry: String,
    source_line: usize,
    byte: usize,
}

#[derive(Default)]
pub(crate) struct BoardState {
    pub(crate) active: bool,
    pub(crate) snapshot: TeamBoardSnapshot,
    pub(crate) team_control: Option<TeamControlSummary>,
    pub(crate) hosts: Vec<super::hosts::HostSummary>,
    pub(crate) selected: Option<String>,
    pub(crate) opened: Option<String>,
    pub(crate) filter: Filter,
    pub(crate) filters_focused: bool,
    pub(crate) conversation_focused: bool,
    pub(crate) split_visible: bool,
    pub(crate) query: String,
    pub(crate) searching: bool,
    pub(crate) editing_topic: bool,
    pub(crate) topic: String,
    pub(crate) composer: Option<ChatComposer>,
    pub(crate) composing: Option<String>,
    pub(crate) drafts: HashMap<String, ComposerDraft>,
    pub(crate) draft_revisions: HashMap<String, u64>,
    pub(crate) pending: HashMap<String, PendingDraft>,
    pub(crate) pending_reads: HashMap<String, PendingRead>,
    pub(crate) pending_subscriptions: HashMap<String, subscriptions::PendingSubscription>,
    pub(crate) topics_open: bool,
    pub(crate) selected_topic: Option<String>,
    pub(crate) topic_filter: Option<String>,
    pub(crate) topics_scroll: usize,
    pub(crate) topic_preview_focused: bool,
    pub(crate) topic_preview_scroll: usize,
    pub(crate) topic_preview_max_scroll: usize,
    pub(crate) topic_preview_page_height: usize,
    pub(crate) feed_positions: HashMap<Option<String>, (Option<String>, usize)>,
    pub(crate) followers: bool,
    pub(crate) reply_targets: HashMap<String, String>,
    pub(crate) feed_scroll: usize,
    pub(crate) scroll: usize,
    pub(crate) at_bottom: bool,
    pub(crate) scroll_width: u16,
    pub(crate) scroll_anchor: Option<ScrollAnchor>,
    pub(crate) viewed_lines: HashMap<(String, u16), usize>,
    pub(crate) latest_seen: HashMap<String, i64>,
    pub(crate) unread_boundary: HashMap<String, i64>,
    pub(crate) read_requested: HashMap<String, i64>,
    /// Rendering records geometry only. App consumes this after a successful focused frame.
    pub(crate) displayed: Option<DisplayedEntries>,
    pub(crate) notice: Option<String>,
    pub(crate) completion: Option<ViewCompletion>,
    pub(crate) page_height: usize,
}

impl BoardState {
    /// Board actions address the region that owns keyboard focus.
    pub(crate) fn focused_conversation(&self) -> Option<&String> {
        if self.conversation_focused && !self.filters_focused {
            self.opened.as_ref()
        } else {
            self.selected.as_ref()
        }
    }

    pub(crate) fn update_team(&mut self, snapshot: &codex_team::TeamSnapshot) {
        // Control and host acknowledgments can change without a new Board revision.
        self.team_control = Some(TeamControlSummary::from(snapshot));
        self.hosts = snapshot
            .servers
            .iter()
            .map(|server| super::hosts::HostSummary {
                key: crate::app::agents_hosts::HostKey {
                    machine_id: server.machine_id.clone(),
                    server_id: server.server_id.clone(),
                },
                label: format!("{} / {}", server.machine_label, server.server_label),
                connected: server.connected,
            })
            .collect();
        self.update(snapshot.board.clone());
    }

    pub(crate) fn update(&mut self, snapshot: TeamBoardSnapshot) {
        if snapshot.revision < self.snapshot.revision {
            return;
        }
        if let Some(composer) = self.composer.as_mut() {
            composer.set_board_mentions(&snapshot.members);
        }
        self.snapshot = snapshot;
        // A purged conversation must return to the feed instead of retaining an empty reader.
        if self.opened.as_ref().is_some_and(|opened| {
            !self
                .snapshot
                .entries
                .iter()
                .any(|entry| &entry.conversation_id == opened)
        }) {
            self.opened = None;
            self.conversation_focused = false;
            self.scroll = 0;
            self.scroll_anchor = None;
        }
        self.confirm_subscriptions();
        self.reconcile_topic_selection();
        for pending in self.pending_reads.values_mut() {
            if self.snapshot.reads.iter().any(|read| {
                read.conversation_id == pending.conversation
                    && read.identity == TeamBoardIdentity::User
                    && read.append_seq >= pending.append_seq
            }) {
                pending.confirmed = true;
            }
        }
        let roots = self.roots();
        if !roots
            .iter()
            .any(|entry| Some(&entry.conversation_id) == self.selected.as_ref())
        {
            self.selected = roots.first().map(|entry| entry.conversation_id.clone());
        }
    }

    pub(crate) fn following(&self, id: &str) -> bool {
        self.snapshot.followers.iter().any(|follower| {
            follower.conversation_id == id && follower.identity == TeamBoardIdentity::User
        })
    }

    pub(crate) fn read_seq(&self, id: &str) -> i64 {
        self.snapshot
            .reads
            .iter()
            .find(|read| read.conversation_id == id && read.identity == TeamBoardIdentity::User)
            .map_or(0, |read| read.append_seq)
    }

    pub(crate) fn unread(&self, id: &str) -> bool {
        let read = self.read_seq(id);
        self.snapshot.entries.iter().any(|entry| {
            entry.conversation_id == id
                && entry.append_seq > read
                && entry.author.identity != TeamBoardIdentity::User
        })
    }

    pub(crate) fn entries(&self, id: &str) -> Vec<&TeamBoardEntry> {
        let mut entries: Vec<_> = self
            .snapshot
            .entries
            .iter()
            .filter(|entry| entry.conversation_id == id)
            .collect();
        entries.sort_by_key(|entry| entry.append_seq);
        entries
    }

    pub(crate) fn roots(&self) -> Vec<&TeamBoardEntry> {
        let mut roots: Vec<_> = self
            .snapshot
            .entries
            .iter()
            .filter(|entry| entry.message.id == entry.conversation_id)
            .filter(|root| {
                let entries = self.entries(&root.conversation_id);
                let filter = match self.filter {
                    Filter::All => true,
                    Filter::Following => {
                        self.following(&root.conversation_id)
                            || root.message.topic.as_ref().is_some_and(|topic| {
                                self.subscription_mode(
                                    &TeamBoardSubscriptionTarget::Topic {
                                        topic: topic.clone(),
                                    },
                                    &TeamBoardIdentity::User,
                                ) != TeamBoardSubscriptionMode::Off
                            })
                    }
                    Filter::Mentions => entries.iter().any(|entry| {
                        entry
                            .mentions
                            .iter()
                            .any(|mention| mention.identity == TeamBoardIdentity::User)
                    }),
                };
                filter
                    && self
                        .topic_filter
                        .as_ref()
                        .is_none_or(|topic| root.message.topic.as_ref() == Some(topic))
                    && self.query.split_whitespace().all(|word| {
                        if let Some(topic) = word.strip_prefix('#') {
                            root.message.topic.as_deref().is_some_and(|value| {
                                value.trim_start_matches('#').eq_ignore_ascii_case(topic)
                            })
                        } else {
                            let word = word.to_lowercase();
                            entries.iter().any(|entry| {
                                entry.message.body.to_lowercase().contains(&word)
                                    || entry.author.name.to_lowercase().contains(&word)
                            })
                        }
                    })
            })
            .collect();
        roots.sort_by_key(|root| {
            std::cmp::Reverse(
                self.entries(&root.conversation_id)
                    .last()
                    .map_or(root.append_seq, |entry| entry.append_seq),
            )
        });
        roots
    }

    pub(crate) fn open_selected(&mut self) {
        if let Some(id) = self.selected.clone() {
            let boundary = self.read_seq(&id);
            self.unread_boundary.insert(id.clone(), boundary);
            self.viewed_lines
                .retain(|(conversation, _), _| conversation != &id);
            self.opened = Some(id);
            self.conversation_focused = true;
            self.filters_focused = false;
            self.scroll = 0;
            self.scroll_width = 0;
            self.scroll_anchor = None;
            self.at_bottom = false;
            self.followers = false;
        }
    }

    pub(crate) fn save_draft(&mut self) {
        if let (Some(key), Some(composer)) = (&self.composing, &self.composer) {
            self.drafts.insert(key.clone(), composer.snapshot_draft());
        }
    }

    pub(crate) fn compose(&mut self, key: String) {
        self.save_draft();
        if let Some(composer) = &mut self.composer {
            composer.restore_draft(self.drafts.get(&key).cloned().unwrap_or_default());
            composer.resume_text_entry();
        }
        self.composing = Some(key);
        self.followers = false;
    }

    pub(crate) fn take_frame_read(&mut self, focused: bool) -> Option<(String, i64)> {
        let displayed = self.displayed.take()?;
        if !focused || !self.active {
            return None;
        }
        let read = self.read_seq(&displayed.conversation).max(
            self.read_requested
                .get(&displayed.conversation)
                .copied()
                .unwrap_or_default(),
        );
        let viewed = self
            .viewed_lines
            .entry((displayed.conversation.clone(), displayed.width))
            .or_default();
        if displayed.start <= *viewed {
            *viewed = (*viewed).max(displayed.end);
        }
        if self
            .pending_reads
            .values()
            .any(|pending| pending.conversation == displayed.conversation && !pending.confirmed)
        {
            return None;
        }
        let last = displayed
            .entries
            .into_iter()
            .filter(|(seq, end)| *seq > read && *end <= *viewed)
            .map(|(seq, _)| seq)
            .max()?;
        self.read_requested
            .insert(displayed.conversation.clone(), last);
        Some((displayed.conversation, last))
    }

    pub(crate) fn take_confirmed_failed_reads(&mut self) -> Vec<String> {
        let ids: Vec<_> = self
            .pending_reads
            .iter()
            .filter(|(_, pending)| pending.confirmed && pending.failed)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            self.pending_reads.remove(id);
        }
        if !ids.is_empty()
            && self
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Read state"))
        {
            self.notice = None;
        }
        ids
    }

    /// A coordinator pre-write rejection releases only an editable draft's operation lock.
    /// Read markers remain explicitly retryable so invalid state cannot spin on every redraw.
    pub(crate) fn operation_rejected(&mut self, operation_id: &str, reason: &str) -> bool {
        if self.pending_subscriptions.remove(operation_id).is_some() {
            self.notice = Some(format!("Subscription rejected: {reason}"));
            return false;
        }
        if self.pending_reads.contains_key(operation_id) {
            self.operation_finished(operation_id, false);
            self.notice = Some(format!("Read state rejected: {reason} · Ctrl+R retries"));
            return true;
        }
        let key = self
            .pending
            .iter()
            .find_map(|(key, pending)| (pending.operation_id == operation_id).then(|| key.clone()));
        if let Some(key) = key {
            self.pending.remove(&key);
            self.notice = Some(format!(
                "Post rejected: {reason} · edit the draft and send again"
            ));
        } else {
            self.notice = Some(format!("Board action rejected: {reason}"));
        }
        false
    }

    pub(crate) fn operation_finished(&mut self, operation_id: &str, succeeded: bool) -> bool {
        if let Some(pending) = self.pending_subscriptions.get_mut(operation_id) {
            pending.failed = !succeeded;
            if !succeeded {
                self.notice =
                    Some("Subscription outcome unknown · Ctrl+R retries the same change".into());
            }
            self.confirm_subscriptions();
            return succeeded;
        }
        if let Some(pending) = self.pending_reads.get_mut(operation_id) {
            if succeeded || pending.confirmed {
                let requested = self
                    .read_requested
                    .entry(pending.conversation.clone())
                    .or_default();
                *requested = (*requested).max(pending.append_seq);
                self.pending_reads.remove(operation_id);
                if self
                    .notice
                    .as_deref()
                    .is_some_and(|notice| notice.starts_with("Read state"))
                {
                    self.notice = None;
                }
                return true;
            }
            pending.failed = true;
            if self
                .read_requested
                .get(&pending.conversation)
                .is_some_and(|requested| *requested <= pending.append_seq)
            {
                self.read_requested.remove(&pending.conversation);
            }
            self.notice = Some("Read state is not synchronized · Ctrl+R retries; reopening this conversation also retries".into());
            return false;
        }
        let key = self
            .pending
            .iter()
            .find_map(|(key, pending)| (pending.operation_id == operation_id).then(|| key.clone()));
        let Some(key) = key else { return succeeded };
        if !succeeded {
            if let Some(pending) = self.pending.get_mut(&key) {
                pending.failed = true;
            }
            self.notice = Some("Send outcome unknown · Ctrl+R retries the same message".into());
            return false;
        }
        let Some(pending) = self.pending.remove(&key) else {
            return succeeded;
        };
        if self.draft_revisions.get(&key).copied().unwrap_or_default() == pending.revision {
            self.drafts.remove(&key);
            if self.composing.as_ref() == Some(&key) {
                if let Some(composer) = &mut self.composer {
                    composer.restore_draft(ComposerDraft::default());
                }
                self.composing = None;
            }
        }
        self.notice = Some("Posted · notifications follow team Pause/Stop controls".into());
        true
    }
}

pub(crate) struct BoardView {
    pub(crate) state: Arc<Mutex<BoardState>>,
    pub(crate) sender: AppEventSender,
}

impl BoardView {
    fn state(&self) -> MutexGuard<'_, BoardState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn request(&self, request: TeamBoardParams) {
        self.sender
            .send(AppEvent::AgentsTeamRequest(TeamRequest::Board { request }));
    }
}
