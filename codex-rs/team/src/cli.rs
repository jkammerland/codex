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
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reply to the original sender, board topic or Gitea conversation.
    Reply {
        message_id: String,
        body: String,
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
    Retry {
        message_id: String,
    },
    /// Serve the agent-only MCP interface over stdio, bound to one thread.
    Mcp(crate::mcp::McpCommand),
    /// Serve privileged operator controls over stdio using the manifest token.
    ControlMcp,
}

#[derive(Clone, Debug, ValueEnum)]
enum MessageKind {
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
        thread_id: parts[2].into(),
    })
}

pub(crate) async fn run(command: TeamCommand) -> Result<()> {
    if let Command::Mcp(mcp) = command.command {
        return crate::mcp::run(mcp).await;
    }
    let manifest = command
        .manifest
        .context("--manifest is required for operator commands")?;
    if matches!(command.command, Command::Serve) {
        return crate::http::serve(Manifest::load(&manifest).await?).await;
    }
    let client = TeamClient::from_manifest(&manifest).await?;
    if matches!(command.command, Command::ControlMcp) {
        return crate::operator_mcp::run(client).await;
    }
    let request = match command.command {
        Command::Reply { message_id, body } => TeamRequest::Reply { message_id, body },
        Command::AgentCredential { address } => TeamRequest::AgentCredential { address },
        Command::GiteaInspect { repository } => TeamRequest::GiteaInspect { repository },
        Command::Status => TeamRequest::Snapshot,
        Command::Enroll { address, alias } => TeamRequest::Enroll { address, alias },
        Command::Send {
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
        Command::Post { topic, body } => TeamRequest::Post { topic, body },
        Command::Subscribe {
            address,
            topic,
            disable,
        } => TeamRequest::Subscribe {
            address,
            topic,
            enabled: !disable,
        },
        Command::Pause => TeamRequest::Control {
            mode: TeamExecutionMode::Paused,
        },
        Command::Resume => TeamRequest::Control {
            mode: TeamExecutionMode::Running,
        },
        Command::Stop => TeamRequest::Control {
            mode: TeamExecutionMode::Stopped,
        },
        Command::Models { machine, server } => TeamRequest::Models {
            machine_id: machine,
            server_id: server,
        },
        Command::SetModel {
            address,
            model,
            effort,
        } => TeamRequest::SetModel {
            address,
            model,
            effort,
        },
        Command::Retry { message_id } => TeamRequest::Retry { message_id },
        Command::Serve | Command::Mcp(_) | Command::ControlMcp => {
            unreachable!("long-running commands were handled above")
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&client.request(request).await?)?
    );
    Ok(())
}
