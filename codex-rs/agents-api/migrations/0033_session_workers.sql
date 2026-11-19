-- Run mode's worker pool: the managed worker each session's threads live on,
-- chosen on the session's first input. A session without a row belongs to
-- worker 0, which is also the only worker without a pool. A deleted session's
-- thread is deleted on the worker recorded with it in the cleanup queue.
CREATE TABLE session_workers (
    session_id TEXT PRIMARY KEY,
    worker INTEGER NOT NULL
);
CREATE INDEX session_workers_worker ON session_workers (worker);
ALTER TABLE session_cleanup ADD COLUMN worker INTEGER NOT NULL DEFAULT 0;
