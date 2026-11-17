-- When each model response finished, as the service observed its usage
-- report, and its cached input and reasoning tokens, so each response becomes
-- its own span. Responses recorded before this migration have none, and their
-- turns' spans are inferred from items.
ALTER TABLE generations ADD COLUMN completed_ms INTEGER;
ALTER TABLE generations ADD COLUMN cached_tokens INTEGER;
ALTER TABLE generations ADD COLUMN reasoning_tokens INTEGER;

-- The response that issued each tool call, as the number of responses the
-- turn had finished when the model's call arrived. Codex reports a call's
-- item when the call starts running, which can be after its response's usage.
CREATE TABLE response_calls (
    session_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    response INTEGER NOT NULL,
    PRIMARY KEY (session_id, turn_id, call_id)
);
