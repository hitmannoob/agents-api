-- How many files and setup commands a managed environment was created with.
-- Their contents are confidential and kept only in the secrets store, so a
-- reset without a snapshot can tell a setup it can rebuild from one it lost.
ALTER TABLE environments ADD COLUMN setup_count INTEGER NOT NULL DEFAULT 0;
