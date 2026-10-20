use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

/// Immutable accepted mail. The runtime owns the versioned kind and JSON payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailEnvelope {
    pub message_id: String,
    pub root_id: ThreadId,
    pub sender_id: ThreadId,
    pub recipient_id: ThreadId,
    pub kind: String,
    pub payload_json: String,
    pub wake: bool,
    pub task_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailRecord {
    pub envelope: AgentMailEnvelope,
    pub sequence: i64,
    pub recorded_turn_id: Option<String>,
    pub submitted_turn_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMailAcceptance {
    pub record: AgentMailRecord,
    pub inserted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTaskStatus {
    Queued,
    Running,
    Held,
    CapacityUnavailable,
    Completed,
    Failed,
    Cancelled,
    Unavailable,
}

impl AgentTaskStatus {
    pub fn is_terminal(self) -> bool {
        match self {
            Self::Queued
            | Self::Running
            | Self::Held
            | Self::CapacityUnavailable
            | Self::Unavailable => false,
            Self::Completed | Self::Failed | Self::Cancelled => true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTaskRecord {
    pub task_id: String,
    pub root_id: ThreadId,
    pub owner_id: ThreadId,
    pub target_id: ThreadId,
    pub name: Option<String>,
    pub status: AgentTaskStatus,
    /// Most recent selected attempt; only Running indicates server-accepted input.
    pub turn_id: Option<String>,
    pub result_ref: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentTaskFilter {
    pub task_ids: Vec<String>,
    pub owner_id: Option<ThreadId>,
    pub target_id: Option<ThreadId>,
    pub statuses: Vec<AgentTaskStatus>,
}

impl AgentTaskFilter {
    /// Empty collections include all values of that field.
    pub fn matches(&self, task: &AgentTaskRecord) -> bool {
        (self.task_ids.is_empty() || self.task_ids.contains(&task.task_id))
            && self.owner_id.is_none_or(|id| id == task.owner_id)
            && self.target_id.is_none_or(|id| id == task.target_id)
            && (self.statuses.is_empty() || self.statuses.contains(&task.status))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentTaskSnapshot {
    pub revision: i64,
    pub tasks: Vec<AgentTaskRecord>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentMailboxSummary {
    pub revision: i64,
    pub pending_mail: i64,
    pub unsettled_tasks: i64,
}

/// Validate stable task ownership independently of execution progress.
pub fn validate_agent_task_identity(
    previous: &AgentTaskRecord,
    task: &AgentTaskRecord,
) -> Result<(), std::io::Error> {
    if previous.task_id != task.task_id
        || previous.root_id != task.root_id
        || previous.owner_id != task.owner_id
        || previous.target_id != task.target_id
        || previous.name != task.name
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "task identity cannot change",
        ));
    }
    Ok(())
}

/// Keep immutable identity and terminal results consistent across store adapters.
pub fn validate_agent_task_update(
    previous: &AgentTaskRecord,
    task: &AgentTaskRecord,
) -> Result<(), std::io::Error> {
    validate_agent_task_identity(previous, task)?;
    if previous.status.is_terminal() && previous != task {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "settled task result cannot change",
        ));
    }
    Ok(())
}

/// Validate that terminal completion mail is attributed and addressed to its owner.
pub fn validate_agent_task_completion(
    task: &AgentTaskRecord,
    completion: &AgentMailEnvelope,
) -> Result<(), std::io::Error> {
    if !task.status.is_terminal()
        || completion.root_id != task.root_id
        || completion.task_id.as_deref() != Some(task.task_id.as_str())
        || completion.sender_id != task.target_id
        || completion.recipient_id != task.owner_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "completion must settle its attributed task from target to owner",
        ));
    }
    Ok(())
}

/// Validate an initial request and its nonterminal task before atomic acceptance.
pub fn validate_agent_task_request(
    task: &AgentTaskRecord,
    request: &AgentMailEnvelope,
) -> Result<(), std::io::Error> {
    if task.status.is_terminal()
        || request.root_id != task.root_id
        || request.task_id.as_deref() != Some(task.task_id.as_str())
        || request.sender_id != task.owner_id
        || request.recipient_id != task.target_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "request must address its attributed nonterminal task from owner to target",
        ));
    }
    Ok(())
}
