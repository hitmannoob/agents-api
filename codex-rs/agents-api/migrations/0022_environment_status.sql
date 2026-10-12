-- What an environment's status needs beyond the live registry, which forgets
-- on restart: whether an executor ever connected (so a restarted service
-- reports disconnected, not pending), and whether input timed out waiting for
-- one (failed, as its event reported, until an executor connects).
ALTER TABLE environments ADD COLUMN connected_once INTEGER NOT NULL DEFAULT 0;
ALTER TABLE environments ADD COLUMN failed INTEGER NOT NULL DEFAULT 0;
