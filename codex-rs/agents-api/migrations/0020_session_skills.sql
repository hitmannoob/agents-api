-- Skills a self-hosted session installs, each resolved to a concrete version
-- when the session was created. The version's contents are pinned under
-- DATA_DIRECTORY/skill_pins/<session_id>/<position>.zip, so deleting the skill
-- or version later does not change what the session installs.
CREATE TABLE session_skills (
    session_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    skill_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (session_id, position)
);
