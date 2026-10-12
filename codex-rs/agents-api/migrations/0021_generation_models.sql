-- The model each turn's responses came from, for trace export: the model the
-- turn requested, replaced when Codex reports the provider rerouted it.
CREATE TABLE turn_models (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    model TEXT NOT NULL,
    PRIMARY KEY (session_id, turn_id)
);

-- The model that produced each response, when known. Responses recorded
-- before this migration have none.
ALTER TABLE generations ADD COLUMN model TEXT;
