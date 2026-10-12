-- Projects own resources, and each API key belongs to exactly one project.
-- Rows that predate projects belong to the default project, which the
-- operator token (`CODEX_AGENTS_API_TOKEN`) authenticates as.
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_seq INTEGER NOT NULL UNIQUE
);

INSERT INTO projects (id, name, created_at, created_seq)
VALUES ('proj_default', 'Default project', CAST(strftime('%s', 'now') AS INTEGER), 1);

-- Only a SHA-256 of each secret is kept; the secret is shown once, at creation.
CREATE TABLE api_keys (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL REFERENCES projects (id),
    name TEXT NOT NULL,
    secret_sha256 TEXT NOT NULL UNIQUE,
    redacted_value TEXT NOT NULL,
    -- A JSON array of permission names.
    permissions TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_seq INTEGER NOT NULL UNIQUE,
    revoked_at INTEGER
);

CREATE INDEX api_keys_by_project ON api_keys (project_id, created_seq);
