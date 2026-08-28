-- Baseline schema: every statement is idempotent so pre-existing databases
-- (created by earlier development builds without a user_version) are adopted
-- as version 1 without changing their data.

CREATE TABLE IF NOT EXISTS artifacts (
    path TEXT PRIMARY KEY COLLATE BINARY,
    group_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    version TEXT NOT NULL,
    file_type TEXT NOT NULL,
    upstream TEXT NOT NULL,
    sha1 TEXT,
    sha256 TEXT,
    etag TEXT,
    last_modified TEXT,
    file_size INTEGER,
    created_at INTEGER NOT NULL,
    last_refresh_attempt INTEGER,
    last_accessed INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS negative_cache (
    path TEXT NOT NULL COLLATE BINARY,
    repository_id TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (path, repository_id)
);

CREATE INDEX IF NOT EXISTS idx_group ON artifacts(group_id);
CREATE INDEX IF NOT EXISTS idx_group_artifact ON artifacts(group_id, artifact_id);
CREATE INDEX IF NOT EXISTS idx_artifact_version ON artifacts(artifact_id, version);
CREATE INDEX IF NOT EXISTS idx_path_nocase ON artifacts(path COLLATE NOCASE);