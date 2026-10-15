CREATE TABLE control (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    coordinator_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 0),
    payload TEXT NOT NULL
);

CREATE TABLE enrollment (
    thread_id TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    credential TEXT NOT NULL
);

CREATE TABLE delivery (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    thread_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    receipt TEXT NOT NULL
);

CREATE INDEX delivery_thread_sequence ON delivery(thread_id, sequence);

CREATE TABLE outbox (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE record (
    namespace TEXT NOT NULL,
    id TEXT NOT NULL,
    payload TEXT NOT NULL,
    PRIMARY KEY(namespace, id)
);
