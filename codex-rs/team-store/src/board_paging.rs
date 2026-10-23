//! Stateless cursors retain immutable content order while read markers advance.

use std::collections::BTreeSet;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct PageCursor {
    version: u8,
    viewer: TeamBoardIdentity,
    pub scope: String,
    revision: i64,
    pub cutoff: i64,
    content: u64,
    membership: Option<u64>,
    pub sequence: i64,
    pub key: String,
}

pub(super) enum Membership {
    None,
    Following,
    Topics,
}

impl PageCursor {
    pub fn open(
        snapshot: &TeamBoardSnapshot,
        viewer: &TeamBoardIdentity,
        scope: String,
        cursor: Option<&str>,
        membership: Membership,
    ) -> Result<(Self, TeamBoardSnapshot)> {
        let state = cursor.map(serde_json::from_str::<Self>).transpose()?;
        let cutoff = state.as_ref().map_or_else(
            || {
                snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.append_seq)
                    .max()
                    .unwrap_or(0)
            },
            |state| state.cutoff,
        );
        let mut prefix = snapshot.clone();
        prefix.entries.retain(|entry| entry.append_seq <= cutoff);
        let mut content = prefix
            .entries
            .iter()
            .map(|entry| (entry.append_seq, entry.message.id.as_str()))
            .collect::<Vec<_>>();
        content.sort_unstable();
        let content = fingerprint(&content);
        let membership = match membership {
            Membership::None => None,
            Membership::Following => {
                let conversations = snapshot
                    .followers
                    .iter()
                    .filter(|follower| {
                        &follower.identity == viewer
                            && follower.mode != TeamBoardSubscriptionMode::Off
                    })
                    .map(|follower| follower.conversation_id.as_str())
                    .collect::<BTreeSet<_>>();
                let topics = snapshot
                    .topic_subscriptions
                    .iter()
                    .filter(|subscription| {
                        &subscription.identity == viewer
                            && subscription.mode != TeamBoardSubscriptionMode::Off
                    })
                    .map(|subscription| subscription.topic.as_str())
                    .collect::<BTreeSet<_>>();
                Some(fingerprint(&(conversations, topics)))
            }
            Membership::Topics => {
                let topics = prefix
                    .entries
                    .iter()
                    .filter_map(|entry| entry.message.topic.as_deref())
                    .chain(
                        snapshot
                            .topic_subscriptions
                            .iter()
                            .map(|subscription| subscription.topic.as_str()),
                    )
                    .collect::<BTreeSet<_>>();
                Some(fingerprint(&topics))
            }
        };
        if let Some(state) = state {
            ensure!(
                state.version == 1 && state.viewer == *viewer && state.scope == scope,
                "cursor belongs to another reader or query; restart from the first page"
            );
            ensure!(
                snapshot.revision >= state.revision,
                "board cache is not synchronized to this cursor yet"
            );
            ensure!(
                state.content == content,
                "board content was removed; restart from the first page"
            );
            ensure!(
                state.membership == membership,
                "board filter membership changed; restart from the first page"
            );
            return Ok((state, prefix));
        }
        Ok((
            Self {
                version: 1,
                viewer: viewer.clone(),
                scope,
                revision: snapshot.revision,
                cutoff,
                content,
                membership,
                sequence: 0,
                key: String::new(),
            },
            prefix,
        ))
    }

    pub fn after(&self, sequence: i64, key: &str) -> Result<String> {
        let mut cursor = self.clone();
        cursor.sequence = sequence;
        cursor.key = key.into();
        Ok(serde_json::to_string(&cursor)?)
    }
}

// This detects a changed immutable projection, not an authorization claim.
// A compiler/version change may invalidate a cursor and require a fresh page.
fn fingerprint(value: &impl Hash) -> u64 {
    let mut hash = DefaultHasher::new();
    value.hash(&mut hash);
    hash.finish()
}
