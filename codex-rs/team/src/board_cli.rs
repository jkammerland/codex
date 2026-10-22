//! Operator commands use explicit participant identities for board notifications.

use anyhow::Context;
use anyhow::Result;
use clap::Args;
use clap::Subcommand;
use clap::ValueEnum;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamBoardFilter;
use codex_app_server_protocol::TeamBoardIdentity;
use codex_app_server_protocol::TeamBoardMention;
use codex_app_server_protocol::TeamBoardParams;
use codex_app_server_protocol::TeamBoardSubscriptionMode;
use codex_app_server_protocol::TeamBoardSubscriptionTarget;

#[derive(Debug, Args)]
pub struct BoardCommand {
    #[command(subcommand)]
    pub action: BoardAction,
}

#[derive(Debug, Subcommand)]
pub enum BoardAction {
    /// Permanently remove whole conversations through exclusive offline maintenance.
    Purge(crate::BoardPurgeArgs),
    /// List conversations, with optional topic, text and personal filters.
    List {
        #[arg(long, value_enum, default_value = "all")]
        filter: BoardFilter,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[command(flatten)]
        page: BoardPage,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
    },
    /// Read a conversation without changing its read cursor.
    Read {
        conversation_id: String,
        #[command(flatten)]
        page: BoardPage,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
    },
    /// Create a post. Its first nonempty line is the feed headline.
    Post {
        body: String,
        #[arg(long, default_value = "general")]
        topic: String,
        /// Bind @LABEL in the body to LABEL=machine/server/thread-id or LABEL=user.
        #[arg(long = "mention", value_name = "LABEL=IDENTITY")]
        mentions: Vec<MentionArgument>,
    },
    /// Reply in a conversation, optionally linking a particular entry.
    Reply {
        conversation_id: String,
        body: String,
        #[arg(long)]
        reply_to: Option<String>,
        /// Bind @LABEL in the body to LABEL=machine/server/thread-id or LABEL=user.
        #[arg(long = "mention", value_name = "LABEL=IDENTITY")]
        mentions: Vec<MentionArgument>,
    },
    /// Follow replies as the human operator, or explicitly subscribe an agent.
    Follow {
        conversation_id: String,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
        /// Remove the subscription; previously queued messages retain their recipients.
        #[arg(long)]
        remove: bool,
    },
    /// List the named participants following a conversation.
    Followers {
        conversation_id: String,
        #[command(flatten)]
        page: BoardPage,
    },
    /// Acknowledge entries through an observed coordinator sequence.
    MarkRead {
        conversation_id: String,
        #[arg(long = "through")]
        append_seq: i64,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
    },
    /// List known topics.
    Topics {
        #[command(flatten)]
        page: BoardPage,
    },
    /// Preview topic activity, named authors, unread counts, and subscription modes.
    TopicOverview {
        #[command(flatten)]
        page: BoardPage,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
    },
    /// Set a passive Watch or automatic Wake subscription for future messages.
    SubscriptionSet {
        #[arg(long, required_unless_present = "topic", conflicts_with = "topic")]
        conversation: Option<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(value_enum)]
        mode: BoardSubscriptionMode,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
    },
    /// Subscribe to new root posts in a topic, without following its replies.
    TopicFollow {
        topic: String,
        #[arg(long, value_parser = super::cli::address)]
        agent: Option<TeamAddress>,
        #[arg(long)]
        remove: bool,
    },
}

#[derive(Clone, Debug, ValueEnum)]
pub enum BoardFilter {
    All,
    Following,
    Mentions,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum BoardSubscriptionMode {
    Off,
    Watch,
    Wake,
}

#[derive(Debug, Args)]
pub struct BoardPage {
    #[arg(long)]
    cursor: Option<String>,
    #[arg(long)]
    limit: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct MentionArgument {
    display: String,
    identity: TeamBoardIdentity,
}

impl std::str::FromStr for MentionArgument {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (label, target) = value
            .rsplit_once('=')
            .ok_or_else(|| "use LABEL=machine/server/thread-id or LABEL=user".to_string())?;
        let label = label.strip_prefix('@').unwrap_or(label);
        if label.trim().is_empty() || label.contains(['\n', '\r']) {
            return Err("mention label must be nonempty and on one line".into());
        }
        let identity = if target == "user" {
            TeamBoardIdentity::User
        } else {
            TeamBoardIdentity::Agent {
                address: super::cli::address(target)?,
            }
        };
        Ok(Self {
            display: format!("@{label}"),
            identity,
        })
    }
}

impl BoardCommand {
    pub(crate) fn into_request(self) -> Result<TeamBoardParams> {
        Ok(match self.action {
            BoardAction::Purge(_) => {
                anyhow::bail!("offline maintenance does not use the agent Board interface")
            }
            BoardAction::List {
                filter,
                query,
                topic,
                page,
                agent,
            } => TeamBoardParams::List {
                filter: match filter {
                    BoardFilter::All => TeamBoardFilter::All,
                    BoardFilter::Following => TeamBoardFilter::Following,
                    BoardFilter::Mentions => TeamBoardFilter::Mentions,
                },
                query,
                topic,
                cursor: page.cursor,
                limit: page.limit,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
            },
            BoardAction::Read {
                conversation_id,
                page,
                agent,
            } => TeamBoardParams::Read {
                conversation_id,
                cursor: page.cursor,
                limit: page.limit,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
            },
            BoardAction::Post {
                body,
                topic,
                mentions,
            } => TeamBoardParams::Post {
                mentions: bind_mentions(&body, mentions)?,
                topic,
                body,
            },
            BoardAction::Reply {
                conversation_id,
                body,
                reply_to,
                mentions,
            } => TeamBoardParams::Reply {
                mentions: bind_mentions(&body, mentions)?,
                conversation_id,
                reply_to,
                body,
            },
            BoardAction::Follow {
                conversation_id,
                agent,
                remove,
            } => TeamBoardParams::Follow {
                conversation_id,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
                enabled: !remove,
            },
            BoardAction::Followers {
                conversation_id,
                page,
            } => TeamBoardParams::Followers {
                conversation_id,
                cursor: page.cursor,
                limit: page.limit,
            },
            BoardAction::MarkRead {
                conversation_id,
                append_seq,
                agent,
            } => TeamBoardParams::MarkRead {
                conversation_id,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
                append_seq,
            },
            BoardAction::Topics { page } => TeamBoardParams::Topics {
                cursor: page.cursor,
                limit: page.limit,
            },
            BoardAction::TopicOverview { page, agent } => TeamBoardParams::TopicOverview {
                cursor: page.cursor,
                limit: page.limit,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
            },
            BoardAction::SubscriptionSet {
                conversation,
                topic,
                mode,
                agent,
            } => {
                let target = if let Some(conversation_id) = conversation {
                    TeamBoardSubscriptionTarget::Conversation { conversation_id }
                } else {
                    TeamBoardSubscriptionTarget::Topic {
                        topic: topic.context("a conversation or topic is required")?,
                    }
                };
                TeamBoardParams::SubscriptionSet {
                    target,
                    identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
                    mode: match mode {
                        BoardSubscriptionMode::Off => TeamBoardSubscriptionMode::Off,
                        BoardSubscriptionMode::Watch => TeamBoardSubscriptionMode::Watch,
                        BoardSubscriptionMode::Wake => TeamBoardSubscriptionMode::Wake,
                    },
                }
            }
            BoardAction::TopicFollow {
                topic,
                agent,
                remove,
            } => TeamBoardParams::TopicFollow {
                topic,
                identity: agent.map(|address| TeamBoardIdentity::Agent { address }),
                enabled: !remove,
            },
        })
    }
}

fn bind_mentions(body: &str, arguments: Vec<MentionArgument>) -> Result<Vec<TeamBoardMention>> {
    let mut mentions = Vec::new();
    for argument in arguments {
        let mut found = false;
        for (start, matched) in body.match_indices(&argument.display) {
            // A selected label must not bind a prefix of another handle or an email.
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
                identity: argument.identity.clone(),
                display: argument.display.clone(),
                start: start
                    .try_into()
                    .context("mention offset exceeds the wire range")?,
                end: end
                    .try_into()
                    .context("mention offset exceeds the wire range")?,
            };
            if !mentions.contains(&mention) {
                mentions.push(mention);
            }
        }
        anyhow::ensure!(
            found,
            "mention {} was not found as a complete label in the body",
            argument.display
        );
    }
    mentions.sort_by_key(|mention| (mention.start, mention.end));
    anyhow::ensure!(
        mentions.windows(2).all(|pair| pair[0].end <= pair[1].start),
        "mention labels overlap or bind the same text to different identities"
    );
    Ok(mentions)
}

#[cfg(test)]
#[path = "board_cli_tests.rs"]
mod tests;
