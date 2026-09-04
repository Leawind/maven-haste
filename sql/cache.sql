-- name: get_artifact?
--
-- Returns the cached artifact record for an exact path, if any.
--
-- param: path: &str
SELECT path, group_id, artifact_id, version, file_type, upstream, sha1, sha256, etag,
       last_modified, file_size, created_at, last_refresh_attempt, last_accessed, request_count
  FROM artifacts
 WHERE path = :path
/

-- name: case_conflicts?
--
-- Returns paths that differ from :path only by case.
--
-- param: path: &str
SELECT path
  FROM artifacts
 WHERE path = :path COLLATE NOCASE AND path != :path
/

-- name: delete_artifact!
--
-- Deletes the artifacts cache entry for a path.
--
-- param: path: &str
DELETE FROM artifacts WHERE path = :path
/

-- name: delete_negative_path!
--
-- Deletes all negative cache entries for a path.
--
-- param: path: &str
DELETE FROM negative_cache WHERE path = :path
/

-- name: delete_negative_entry!
--
-- Deletes one negative cache entry for a path and repository.
--
-- param: path: &str
-- param: repository_id: &str
DELETE FROM negative_cache WHERE path = :path AND repository_id = :repository_id
/

-- name: negative_entries?
--
-- Returns negative cache entries for a path.
--
-- param: path: &str
SELECT repository_id, observed_at
  FROM negative_cache
 WHERE path = :path
/

-- name: upsert_artifact!
--
-- Inserts a cached artifact or fully replaces the existing record.
--
-- param: path: &str
-- param: group_id: &str
-- param: artifact_id: &str
-- param: version: &str
-- param: file_type: &str
-- param: upstream: &str
-- param: sha1: Option<&str>
-- param: sha256: Option<&str>
-- param: etag: Option<&str>
-- param: last_modified: Option<&str>
-- param: file_size: i64
-- param: created_at: i64
-- param: last_refresh_attempt: Option<i64>
-- param: last_accessed: i64
INSERT INTO artifacts (path, group_id, artifact_id, version, file_type, upstream, sha1, sha256,
                       etag, last_modified, file_size, created_at, last_refresh_attempt,
                       last_accessed)
VALUES (:path, :group_id, :artifact_id, :version, :file_type, :upstream, :sha1, :sha256, :etag,
        :last_modified, :file_size, :created_at, :last_refresh_attempt, :last_accessed)
ON CONFLICT(path) DO UPDATE SET
    group_id = excluded.group_id,
    artifact_id = excluded.artifact_id,
    version = excluded.version,
    file_type = excluded.file_type,
    upstream = excluded.upstream,
    sha1 = excluded.sha1,
    sha256 = excluded.sha256,
    etag = excluded.etag,
    last_modified = excluded.last_modified,
    file_size = excluded.file_size,
    created_at = excluded.created_at,
    last_refresh_attempt = excluded.last_refresh_attempt,
    last_accessed = excluded.last_accessed
/

-- name: upsert_negative_entry!
--
-- Records (or refreshes) a negative cache observation for a path and repository.
--
-- param: path: &str
-- param: repository_id: &str
-- param: observed_at: i64
INSERT INTO negative_cache (path, repository_id, observed_at)
VALUES (:path, :repository_id, :observed_at)
ON CONFLICT(path, repository_id) DO UPDATE SET observed_at = excluded.observed_at
/

-- name: touch_refresh_attempt!
--
-- Updates the last refresh attempt timestamp of an artifact.
--
-- param: path: &str
-- param: timestamp: i64
UPDATE artifacts SET last_refresh_attempt = :timestamp WHERE path = :path
/

-- name: record_hit!
--
-- Counts one request for a path and refreshes its access timestamp in a
-- single write. Paths without a cached record are not counted.
--
-- param: path: &str
-- param: timestamp: i64
UPDATE artifacts SET request_count = request_count + 1, last_accessed = :timestamp WHERE path = :path
/

-- name: records_by_access?
--
-- Returns all artifact records ordered by least recently accessed.
--
SELECT path, group_id, artifact_id, version, file_type, upstream, sha1, sha256, etag,
       last_modified, file_size, created_at, last_refresh_attempt, last_accessed, request_count
  FROM artifacts
 ORDER BY last_accessed, created_at, path
/

-- name: records_with_prefix?
--
-- Returns artifact records matching an exact path or its descendants.
--
-- param: path: &str
-- param: pattern: &str
SELECT path, group_id, artifact_id, version, file_type, upstream, sha1, sha256, etag,
       last_modified, file_size, created_at, last_refresh_attempt, last_accessed, request_count
  FROM artifacts
 WHERE path = :path OR path LIKE :pattern ESCAPE '\'
 ORDER BY path
/

-- name: stats?
--
-- Returns overall cache statistics.
--
SELECT (SELECT COUNT(*) FROM artifacts),
       (SELECT COALESCE(SUM(file_size), 0) FROM artifacts),
       (SELECT COUNT(*) FROM negative_cache)
/

-- name: ping?
--
-- A trivial statement used to verify database connectivity.
--
SELECT 1
/