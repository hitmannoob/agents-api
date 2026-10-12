-- Webhook endpoints belong to the project of the key that created them, and
-- receive only events about that project's sessions. Existing endpoints
-- belong to the default project.
ALTER TABLE webhook_endpoints ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';

CREATE INDEX webhook_endpoints_by_project ON webhook_endpoints (project_id, created_seq);
