-- Skills uploaded through the Skills API (`/v1/skills`), and their immutable
-- numbered versions. Each version's canonical ZIP lives in
-- DATA_DIRECTORY/skills under the version's ID. A skill's name and
-- description are its default version's; its latest version is the highest
-- remaining number. `seq` orders listings and is never reused.
CREATE TABLE skills (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    project_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    default_version INTEGER NOT NULL,
    -- The number the next version takes, so numbers are never reused.
    next_version INTEGER NOT NULL
);

CREATE INDEX skills_by_project ON skills (project_id, seq);

CREATE TABLE skill_versions (
    id TEXT NOT NULL UNIQUE,
    skill_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (skill_id, version)
);
