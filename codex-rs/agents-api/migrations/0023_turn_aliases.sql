-- Public IDs for turns that were queued before Codex started them. A queued
-- turn gets its ID while its input waits for a self-hosted executor; Codex
-- picks its own ID when the turn starts, and the service reports the queued
-- ID in its place.
CREATE TABLE turn_aliases (
    codex_turn_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    public_turn_id TEXT NOT NULL UNIQUE
);
