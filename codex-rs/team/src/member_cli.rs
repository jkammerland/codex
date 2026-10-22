use clap::Args;
use clap::Subcommand;

#[derive(Debug, Args)]
pub struct MemberCommand {
    #[command(subcommand)]
    pub action: MemberAction,
}

#[derive(Debug, Subcommand)]
pub enum MemberAction {
    /// Create an idle durable session and enroll it without starting a turn.
    Create {
        alias: String,
        #[arg(long)]
        machine: String,
        #[arg(long)]
        server: String,
        /// Existing absolute checkout directory on the selected machine.
        #[arg(long)]
        cwd: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
}

impl MemberCommand {
    pub(crate) fn into_request(self) -> crate::TeamRequest {
        match self.action {
            MemberAction::Create {
                alias,
                machine,
                server,
                cwd,
                model,
                effort,
            } => crate::TeamRequest::CreateMember {
                request: crate::TeamMemberCreateRequest {
                    machine_id: machine,
                    server_id: server,
                    alias,
                    cwd,
                    model,
                    effort,
                },
            },
        }
    }
}
