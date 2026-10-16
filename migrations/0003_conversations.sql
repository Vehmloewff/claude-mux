-- A session is one Claude Code conversation, keyed by claude's own id: a
-- `/clear` starts a new session that takes over the process, and the old
-- one points at it. Sessions are described rather than named, and
-- `context` is the first prompt.
UPDATE sessions SET id = claude_session_id;
ALTER TABLE sessions DROP COLUMN claude_session_id;
ALTER TABLE sessions ADD COLUMN continued_as TEXT;
ALTER TABLE sessions RENAME COLUMN name TO description;
ALTER TABLE sessions RENAME COLUMN context TO prompt;
