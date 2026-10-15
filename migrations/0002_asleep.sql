-- Idle sessions whose claude process was stopped to save memory.
ALTER TABLE sessions ADD COLUMN asleep INTEGER NOT NULL DEFAULT 0;
