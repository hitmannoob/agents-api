-- Managed (openai_hosted) environments: the service provisions their
-- executors. `provider` is null for self-hosted environments. The executor
-- authenticates with a connect token accepted for this environment only;
-- only its SHA-256 is kept. `provisioned` is set once the sandbox runs.
ALTER TABLE environments ADD COLUMN provider TEXT;
ALTER TABLE environments ADD COLUMN sandbox_id TEXT;
ALTER TABLE environments ADD COLUMN token_sha256 TEXT;
ALTER TABLE environments ADD COLUMN provisioned INTEGER NOT NULL DEFAULT 0;
