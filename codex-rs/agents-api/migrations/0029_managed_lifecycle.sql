-- Managed environment lifecycle. `snapshot_id` is the provider's latest
-- filesystem snapshot (taken at `snapshot_ms`), which a reset or resume
-- restores. `activity_ms` is when the session was last seen busy;
-- `disconnected_ms` when the executor was last seen to drop. A suspended
-- environment has no sandbox until input resumes it; an expired one's
-- snapshot is gone. `env_count` records how many confidential variables the
-- environment needs restored (their values are in the secrets store).
ALTER TABLE environments ADD COLUMN snapshot_id TEXT;
ALTER TABLE environments ADD COLUMN snapshot_ms INTEGER;
ALTER TABLE environments ADD COLUMN activity_ms INTEGER;
ALTER TABLE environments ADD COLUMN disconnected_ms INTEGER;
ALTER TABLE environments ADD COLUMN reset_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE environments ADD COLUMN suspended_ms INTEGER;
ALTER TABLE environments ADD COLUMN expired INTEGER NOT NULL DEFAULT 0;
ALTER TABLE environments ADD COLUMN env_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE environments ADD COLUMN registry_url TEXT;
