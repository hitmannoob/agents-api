-- Where a managed environment's executor runs commands. Self-hosted
-- environments keep it in their public `workspace_directory`; the public
-- openai_hosted object has no such field, so it is stored here.
ALTER TABLE environments ADD COLUMN workspace_directory TEXT;
