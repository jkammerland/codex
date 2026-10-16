use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use codex_app_server_protocol::TeamAddress;
use codex_app_server_protocol::TeamDeliveryMode;
use codex_app_server_protocol::TeamExecutionMode;
use codex_app_server_protocol::TeamMessageKind;

use crate::Manifest;
use crate::TeamClient;
use crate::TeamRequest;

#[derive(Debug, Parser)]
pub struct TeamCommand {
    /// Runtime team manifest. Paths in it are relative to the manifest.
    #[arg(long, global = true)]
    pub manifest: Option<PathBuf>,
    /// Reuse this identity when retrying a submission whose response was lost.
    #[arg(long, global = true)]
    pub operation_id: Option<String>,
    #[command(subcommand)]
    pub command: TeamAction,
}

#[derive(Debug, Subcommand)]
pub enum TeamAction {
    /// Reply to the original sender, board topic or Gitea conversation.
    Reply {
        message_id: String,
        body: String,
        #[arg(long)]
        safe_boundary: bool,
    },
    /// Read a bound agent credential for an external MCP bridge.
    AgentCredential {
        #[arg(value_parser = address)]
        address: TeamAddress,
    },
    /// Discover the configured Gitea version and repository project API.
    GiteaInspect {
        repository: String,
    },
    /// Run the durable coordinator in the foreground.
    Serve,
    /// Print machine, agent, control and message state as JSON.
    Status,
    /// Enroll an existing root session, or rename its team alias.
    Enroll {
        #[arg(value_parser = address)]
        address: TeamAddress,
        alias: String,
    },
    /// Queue an attributed message. Requests and replies wake idle agents.
    Send {
        #[arg(value_parser = address)]
        recipient: TeamAddress,
        body: String,
        #[arg(long, value_enum, default_value = "request")]
        kind: MessageKind,
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long)]
        safe_boundary: bool,
    },
    /// Publish to a board topic; only explicit subscribers are woken.
    Post {
        topic: String,
        body: String,
    },
    Subscribe {
        #[arg(value_parser = address)]
        address: TeamAddress,
        topic: String,
        #[arg(long)]
        disable: bool,
    },
    /// Hold new execution. Already admitted work may finish.
    Pause,
    /// Release team holds and continue stopped turns in their original sessions.
    Resume,
    /// Interrupt enrolled families and cancel their tracked work.
    Stop,
    /// Query the selected host's available models and effort levels.
    Models {
        machine: String,
        server: String,
    },
    /// Change the next-turn model without replacing or interrupting the agent.
    SetModel {
        #[arg(value_parser = address)]
        address: TeamAddress,
        model: String,
        #[arg(long)]
        effort: Option<String>,
    },
    /// Release an idle, settled enrolled family from team execution.
    Leave {
        #[arg(value_parser = address)]
        address: TeamAddress,
    },
    /// Set each field independently: keep, default, inherit, or value:NAME.
    SelectModel {
        #[arg(value_parser = address)]
        address: TeamAddress,
        #[arg(long, default_value = "keep", value_parser = selection)]
        model: codex_app_server_protocol::TeamSelection,
        #[arg(long, default_value = "keep", value_parser = selection)]
        effort: codex_app_server_protocol::TeamSelection,
    },
    Retry {
        message_id: String,
    },
    /// Serve the agent-only MCP interface over stdio, bound to one thread.
    Mcp(crate::mcp::McpCommand),
    /// Serve privileged operator controls over stdio using the manifest token.
    ControlMcp,
}

#[derive(Clone, Debug, ValueEnum)]
pub enum MessageKind {
    Request,
    Reply,
    Note,
}

pub(crate) fn address(value: &str) -> std::result::Result<TeamAddress, String> {
    let parts = value.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err("use machine/server/thread-id".into());
    }
    uuid::Uuid::parse_str(parts[2]).map_err(|error| format!("invalid thread id: {error}"))?;
    Ok(TeamAddress {
        machine_id: parts[0].into(),
        server_id: parts[1].into(),
        thread_id: uuid::Uuid::parse_str(parts[2])
            .map_err(|error| format!("invalid thread id: {error}"))?
            .to_string(),
    })
}

fn selection(value: &str) -> std::result::Result<codex_app_server_protocol::TeamSelection, String> {
    use codex_app_server_protocol::TeamSelection;
    match value {
        "keep" => Ok(TeamSelection::Keep),
        "default" => Ok(TeamSelection::Default),
        "inherit" => Ok(TeamSelection::Inherit),
        _ => value
            .strip_prefix("value:")
            .filter(|value| !value.is_empty())
            .map(|value| TeamSelection::Value {
                value: value.into(),
            })
            .ok_or_else(|| "use keep, default, inherit, or value:NAME".into()),
    }
}

pub(crate) async fn run(command: TeamCommand) -> Result<()> {
    if let TeamAction::Mcp(mcp) = command.command {
        return crate::mcp::run(mcp).await;
    }
    let manifest = command
        .manifest
        .context("--manifest is required for operator commands")?;
    if matches!(command.command, TeamAction::Serve) {
        return crate::http::serve(Manifest::load(&manifest).await?).await;
    }
    let client = TeamClient::from_manifest(&manifest).await?;
    if matches!(command.command, TeamAction::ControlMcp) {
        return crate::operator_mcp::run(client).await;
    }
    let request = match command.command {
        TeamAction::Reply {
            message_id,
            body,
            safe_boundary,
        } => TeamRequest::Reply {
            message_id,
            body,
            delivery: if safe_boundary {
                TeamDeliveryMode::SafeBoundary
            } else {
                TeamDeliveryMode::NextTurn
            },
        },
        TeamAction::AgentCredential { address } => TeamRequest::AgentCredential { address },
        TeamAction::GiteaInspect { repository } => TeamRequest::GiteaInspect { repository },
        TeamAction::Status => TeamRequest::Snapshot,
        TeamAction::Enroll { address, alias } => TeamRequest::Enroll { address, alias },
        TeamAction::Send {
            recipient,
            body,
            kind,
            reply_to,
            safe_boundary,
        } => TeamRequest::Send {
            recipient,
            body,
            kind: match kind {
                MessageKind::Request => TeamMessageKind::Request,
                MessageKind::Reply => TeamMessageKind::Reply,
                MessageKind::Note => TeamMessageKind::Note,
            },
            reply_to,
            delivery: if safe_boundary {
                TeamDeliveryMode::SafeBoundary
            } else {
                TeamDeliveryMode::NextTurn
            },
        },
        TeamAction::Post { topic, body } => TeamRequest::Post { topic, body },
        TeamAction::Subscribe {
            address,
            topic,
            disable,
        } => TeamRequest::Subscribe {
            address,
            topic,
            enabled: !disable,
        },
        TeamAction::Pause => TeamRequest::Control {
            mode: TeamExecutionMode::Paused,
        },
        TeamAction::Resume => TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        },
        TeamAction::Stop => TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        },
        TeamAction::Models { machine, server } => TeamRequest::Models {
            machine_id: machine,
            server_id: server,
        },
        TeamAction::SetModel {
            address,
            model,
            effort,
        } => TeamRequest::SetModel {
            address,
            model,
            effort,
        },
        TeamAction::Leave { address } => TeamRequest::Leave { address },
        TeamAction::SelectModel {
            address,
            model,
            effort,
        } => TeamRequest::SelectModel {
            address,
            model,
            effort,
        },
        TeamAction::Retry { message_id } => TeamRequest::Retry { message_id },
        TeamAction::Serve | TeamAction::Mcp(_) | TeamAction::ControlMcp => {
            unreachable!("long-running commands were handled above")
        }
    };
    let operation = match command.operation_id {
        Some(operation_id) => crate::TeamOperation {
            operation_id,
            request,
        },
        None => crate::TeamOperation::new(request),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&client.request_operation(&operation).await?)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_delivery_flag_defaults_to_next_turn_and_accepts_safe_boundary() {
        for safe_boundary in [false, true] {
            let mut args = vec!["agents", "reply", "message-id", "Reviewed"];
            if safe_boundary {
                args.push("--safe-boundary");
            }
            let command = TeamCommand::try_parse_from(args).expect("reply arguments are valid");
            assert!(
                matches!(command.command, TeamAction::Reply { safe_boundary: selected, .. } if selected == safe_boundary)
            );
        }
    }
}
