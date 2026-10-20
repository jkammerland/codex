-- Agent delivery and task settlement share the existing queue database so that
-- terminal task state and its completion mail commit together.
CREATE TABLE agent_mailbox_roots (
    root_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE agent_mail (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL UNIQUE,
    root_id TEXT NOT NULL REFERENCES agent_mailbox_roots(root_id) ON DELETE CASCADE,
    recipient_id TEXT NOT NULL,
    envelope_json TEXT NOT NULL,
    recorded_turn_id TEXT,
    submitted_turn_id TEXT
);

CREATE INDEX agent_mail_pending_idx
    ON agent_mail(root_id, recipient_id, sequence) WHERE submitted_turn_id IS NULL;

CREATE TABLE agent_tasks (
    task_id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL REFERENCES agent_mailbox_roots(root_id) ON DELETE CASCADE,
    record_json TEXT NOT NULL,
    terminal INTEGER NOT NULL
);

CREATE INDEX agent_tasks_root_idx ON agent_tasks(root_id);
