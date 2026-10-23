//! Offset-free Board publication resolves immutable recipients before intake.

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_app_server_protocol::*;
use serde_json::Value;

use crate::TeamHostService;

pub(crate) struct Publication {
    pub(crate) body: String,
    pub(crate) topic: Option<String>,
    pub(crate) conversation_id: Option<String>,
    pub(crate) reply_to: Option<String>,
    pub(crate) mentions: Vec<TeamAgentMentionParams>,
}

impl TeamHostService {
    pub(crate) async fn publish(
        &self,
        sender: TeamAddress,
        publication: Publication,
        operation: Option<&(String, String)>,
    ) -> Result<Value> {
        let mut bindings = Vec::new();
        for mention in publication.mentions {
            let (identity, default_display) = match &mention.target {
                TeamAgentTarget::Alias(alias) if alias == "user" => {
                    (TeamBoardIdentity::User, Some("@user".into()))
                }
                target => {
                    let (address, alias) = self.resolve_team_target(target).await?;
                    (
                        TeamBoardIdentity::Agent { address },
                        alias.map(|alias| format!("@{alias}")),
                    )
                }
            };
            let display = mention
                .display
                .or(default_display)
                .context("mention display is required for an address without a known alias")?;
            ensure!(
                display.starts_with('@') && display.len() > 1,
                "mention display must be a non-empty @label"
            );
            bindings.push((identity, display));
        }
        let mentions = bind_mentions(&publication.body, bindings)?;
        let request = if let Some(conversation_id) = publication.conversation_id {
            ensure!(
                publication.topic.is_none(),
                "reply topic is inherited from its conversation"
            );
            TeamBoardParams::Reply {
                conversation_id,
                reply_to: publication.reply_to,
                body: publication.body,
                mentions,
            }
        } else {
            ensure!(
                publication.reply_to.is_none(),
                "replyTo requires conversationId"
            );
            TeamBoardParams::Post {
                topic: publication.topic.context("new root posts require topic")?,
                body: publication.body,
                mentions,
            }
        };
        self.board_action(sender, request, operation).await
    }
}

fn bind_mentions(
    body: &str,
    bindings: Vec<(TeamBoardIdentity, String)>,
) -> Result<Vec<TeamBoardMention>> {
    let mut mentions = Vec::new();
    for (identity, display) in bindings {
        let mut found = false;
        for (start, matched) in body.match_indices(&display) {
            let before = body[..start].chars().next_back();
            let end = start + matched.len();
            let after = body[end..].chars().next();
            if before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '@'))
                || after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
            {
                continue;
            }
            found = true;
            let mention = TeamBoardMention {
                identity: identity.clone(),
                display: display.clone(),
                start: start
                    .try_into()
                    .context("mention offset exceeds wire range")?,
                end: end
                    .try_into()
                    .context("mention offset exceeds wire range")?,
            };
            if !mentions.contains(&mention) {
                mentions.push(mention);
            }
        }
        ensure!(
            found,
            "mention {display} was not found as a complete label in body"
        );
    }
    mentions.sort_by_key(|mention| (mention.start, mention.end));
    ensure!(
        mentions.windows(2).all(|pair| pair[0].end <= pair[1].start),
        "mention labels overlap or bind the same text to different identities"
    );
    Ok(mentions)
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod tests;
