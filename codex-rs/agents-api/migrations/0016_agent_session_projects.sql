-- Agents and sessions belong to the project of the key that created them.
-- Existing rows belong to the default project. `sessions` is checked for the
-- prototype routes and `public_sessions` for the official ones.
ALTER TABLE agents ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';
ALTER TABLE sessions ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';
ALTER TABLE public_sessions ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';

CREATE INDEX agents_by_project ON agents (project_id, created_seq);
CREATE INDEX public_sessions_by_project ON public_sessions (project_id, created_seq);
