-- Vaults and files belong to the project of the key that created them;
-- credentials inherit their vault's project and environments their session's.
-- Existing rows belong to the default project.
ALTER TABLE vaults ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';
ALTER TABLE files ADD COLUMN project_id TEXT NOT NULL DEFAULT 'proj_default';

CREATE INDEX vaults_by_project ON vaults (project_id, created_seq);
CREATE INDEX files_by_project ON files (project_id, seq);
