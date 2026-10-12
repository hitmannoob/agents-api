-- The path Codex addresses each subagent by, such as `/root/researcher`, so
-- messages between agents name their sender and recipient by public ID.
-- Subagents registered before this migration fill it in when next seen.
ALTER TABLE subagents ADD COLUMN agent_path TEXT;
