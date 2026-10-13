CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT,
    cwd TEXT NOT NULL,
    context TEXT NOT NULL,
    status TEXT NOT NULL,
    status_detail TEXT,
    title TEXT,
    claude_session_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    exit_code INTEGER,
    turns INTEGER NOT NULL,
    last_message TEXT,
    -- JSON array of strings.
    claude_args TEXT NOT NULL,
    auto_trust INTEGER NOT NULL,
    -- JSON array of BackgroundTask.
    background TEXT NOT NULL
);
