use std::path::Path;

use deadpool_sqlite::rusqlite::{OptionalExtension, params};
use deadpool_sqlite::{Config, Hook, HookError, Pool, Runtime};

use crate::error::AppError;

const CONNECTION_PRAGMAS: &str = r#"
PRAGMA synchronous=OFF;
"#;

const SCHEMA: &str = r#"
PRAGMA journal_mode=DELETE;

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
    last_refresh_attempt INTEGER
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
"#;

#[derive(Clone)]
pub struct Database {
    pool: Pool,
}

#[derive(Clone, Debug)]
pub struct ArtifactRecord {
    pub path: String,
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub file_type: String,
    pub upstream: String,
    pub sha1: Option<String>,
    pub sha256: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub file_size: i64,
    pub created_at: i64,
    pub last_refresh_attempt: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegativeCacheEntry {
    pub repository_id: String,
    pub observed_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseStats {
    pub files: u64,
    pub total_size: u64,
    pub negative_entries: u64,
}

impl Database {
    pub async fn open(path: &Path) -> Result<Self, AppError> {
        let config = Config::new(path);
        let pool = config
            .builder(Runtime::Tokio1)
            .expect("deadpool-sqlite configuration is infallible")
            .post_create(Hook::async_fn(|connection, _metrics| {
                Box::pin(async move {
                    connection
                        .interact(|connection| connection.execute_batch(CONNECTION_PRAGMAS))
                        .await
                        .map_err(|error| HookError::message(error.to_string()))?
                        .map_err(HookError::Backend)
                })
            }))
            .build()
            .map_err(|error| AppError::Runtime(format!("failed to create SQLite pool: {error}")))?;
        let database = Self { pool };
        database.initialize().await?;
        Ok(database)
    }

    async fn initialize(&self) -> Result<(), AppError> {
        let connection = self.pool.get().await.map_err(|error| {
            AppError::Runtime(format!("failed to open SQLite database: {error}"))
        })?;
        connection
            .interact(|connection| connection.execute_batch(SCHEMA))
            .await
            .map_err(|error| AppError::Runtime(format!("SQLite worker failed: {error}")))?
            .map_err(|error| AppError::Runtime(format!("failed to initialize SQLite: {error}")))?;
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<Option<ArtifactRecord>, AppError> {
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                connection
                    .query_row(
                        "SELECT path, group_id, artifact_id, version, file_type, upstream, sha1, \
                         sha256, etag, last_modified, file_size, created_at, last_refresh_attempt \
                         FROM artifacts WHERE path = ?1",
                        [path],
                        |row| {
                            Ok(ArtifactRecord {
                                path: row.get(0)?,
                                group_id: row.get(1)?,
                                artifact_id: row.get(2)?,
                                version: row.get(3)?,
                                file_type: row.get(4)?,
                                upstream: row.get(5)?,
                                sha1: row.get(6)?,
                                sha256: row.get(7)?,
                                etag: row.get(8)?,
                                last_modified: row.get(9)?,
                                file_size: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                                created_at: row.get(11)?,
                                last_refresh_attempt: row.get(12)?,
                            })
                        },
                    )
                    .optional()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn case_conflicts(&self, path: &str) -> Result<Vec<String>, AppError> {
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT path FROM artifacts WHERE path = ?1 COLLATE NOCASE AND path != ?1",
                )?;
                let rows = statement.query_map([path], |row| row.get(0))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn delete_paths(&self, paths: Vec<String>) -> Result<(), AppError> {
        if paths.is_empty() {
            return Ok(());
        }
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let transaction = connection.transaction()?;
                for path in paths {
                    transaction.execute("DELETE FROM artifacts WHERE path = ?1", [&path])?;
                    transaction.execute("DELETE FROM negative_cache WHERE path = ?1", [&path])?;
                }
                transaction.commit()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn upsert_many(&self, records: Vec<ArtifactRecord>) -> Result<(), AppError> {
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let transaction = connection.transaction()?;
                for record in records {
                    upsert_record(&transaction, record)?;
                }
                transaction.commit()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn negative_entries(&self, path: &str) -> Result<Vec<NegativeCacheEntry>, AppError> {
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT repository_id, observed_at FROM negative_cache WHERE path = ?1",
                )?;
                let rows = statement.query_map([path], |row| {
                    Ok(NegativeCacheEntry {
                        repository_id: row.get(0)?,
                        observed_at: row.get(1)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn upsert_negative_entries(
        &self,
        path: &str,
        repository_ids: Vec<String>,
        observed_at: i64,
    ) -> Result<(), AppError> {
        if repository_ids.is_empty() {
            return Ok(());
        }
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let transaction = connection.transaction()?;
                for repository_id in repository_ids {
                    transaction.execute(
                        "INSERT INTO negative_cache (path, repository_id, observed_at) \
                         VALUES (?1, ?2, ?3) ON CONFLICT(path, repository_id) DO UPDATE SET \
                         observed_at = excluded.observed_at",
                        params![path, repository_id, observed_at],
                    )?;
                }
                transaction.commit()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn delete_negative_entry(
        &self,
        path: &str,
        repository_id: &str,
    ) -> Result<(), AppError> {
        let path = path.to_owned();
        let repository_id = repository_id.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                connection.execute(
                    "DELETE FROM negative_cache WHERE path = ?1 AND repository_id = ?2",
                    params![path, repository_id],
                )?;
                Ok(())
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn delete_negative_entries(
        &self,
        path: &str,
        repository_ids: Vec<String>,
    ) -> Result<(), AppError> {
        if repository_ids.is_empty() {
            return Ok(());
        }
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                let transaction = connection.transaction()?;
                for repository_id in repository_ids {
                    transaction.execute(
                        "DELETE FROM negative_cache WHERE path = ?1 AND repository_id = ?2",
                        params![path, repository_id],
                    )?;
                }
                transaction.commit()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn touch_refresh_attempt(&self, path: &str, timestamp: i64) -> Result<(), AppError> {
        let path = path.to_owned();
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                connection.execute(
                    "UPDATE artifacts SET last_refresh_attempt = ?2 WHERE path = ?1",
                    params![path, timestamp],
                )?;
                Ok(())
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn ping(&self) -> Result<(), AppError> {
        let connection = self.connection().await?;
        connection
            .interact(|connection| connection.query_row("SELECT 1", [], |_| Ok(())))
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn stats(&self) -> Result<DatabaseStats, AppError> {
        let connection = self.connection().await?;
        connection
            .interact(|connection| {
                connection.query_row(
                    "SELECT (SELECT COUNT(*) FROM artifacts), \
                     (SELECT COALESCE(SUM(file_size), 0) FROM artifacts), \
                     (SELECT COUNT(*) FROM negative_cache)",
                    [],
                    |row| {
                        Ok(DatabaseStats {
                            files: row.get::<_, u64>(0)?,
                            total_size: row.get::<_, u64>(1)?,
                            negative_entries: row.get::<_, u64>(2)?,
                        })
                    },
                )
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    async fn connection(&self) -> Result<deadpool_sqlite::Connection, AppError> {
        self.pool
            .get()
            .await
            .map_err(|error| AppError::Runtime(format!("failed to get SQLite connection: {error}")))
    }
}

fn upsert_record(
    connection: &deadpool_sqlite::rusqlite::Connection,
    record: ArtifactRecord,
) -> Result<(), deadpool_sqlite::rusqlite::Error> {
    connection.execute(
        "INSERT INTO artifacts (path, group_id, artifact_id, version, file_type, upstream, sha1, \
         sha256, etag, last_modified, file_size, created_at, last_refresh_attempt) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) \
         ON CONFLICT(path) DO UPDATE SET group_id = excluded.group_id, \
         artifact_id = excluded.artifact_id, version = excluded.version, \
         file_type = excluded.file_type, upstream = excluded.upstream, sha1 = excluded.sha1, \
         sha256 = excluded.sha256, etag = excluded.etag, \
         last_modified = excluded.last_modified, file_size = excluded.file_size, \
         created_at = excluded.created_at, last_refresh_attempt = excluded.last_refresh_attempt",
        params![
            record.path,
            record.group_id,
            record.artifact_id,
            record.version,
            record.file_type,
            record.upstream,
            record.sha1,
            record.sha256,
            record.etag,
            record.last_modified,
            record.file_size,
            record.created_at,
            record.last_refresh_attempt,
        ],
    )?;
    Ok(())
}

fn worker_error(error: deadpool_sqlite::InteractError) -> AppError {
    AppError::Runtime(format!("SQLite worker failed: {error}"))
}

fn sqlite_error(error: deadpool_sqlite::rusqlite::Error) -> AppError {
    AppError::Runtime(format!("SQLite operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn creates_artifact_schema() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(&directory.path().join("cache.db"))
            .await
            .unwrap();
        let connection = database.pool.get().await.unwrap();
        let table = connection
            .interact(|connection| {
                connection
                    .query_row(
                        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'artifacts'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(table.as_deref(), Some("artifacts"));
    }

    #[tokio::test]
    async fn configures_every_pooled_connection() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(&directory.path().join("cache.db"))
            .await
            .unwrap();
        let first = database.pool.get().await.unwrap();
        let second = database.pool.get().await.unwrap();

        for connection in [first, second] {
            let synchronous = connection
                .interact(|connection| {
                    connection.query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(synchronous, 0);
        }
    }

    #[tokio::test]
    async fn round_trips_artifact_and_finds_case_conflicts() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(&directory.path().join("cache.db"))
            .await
            .unwrap();
        let record = ArtifactRecord {
            path: "Com/Example/demo.jar".into(),
            group_id: "Com.Example".into(),
            artifact_id: "demo".into(),
            version: "1.0".into(),
            file_type: "jar".into(),
            upstream: "central".into(),
            sha1: None,
            sha256: None,
            etag: Some("tag".into()),
            last_modified: None,
            file_size: 42,
            created_at: 123,
            last_refresh_attempt: None,
        };
        database.upsert_many(vec![record.clone()]).await.unwrap();

        assert_eq!(
            database.get(&record.path).await.unwrap().unwrap().file_size,
            42
        );
        assert_eq!(
            database
                .case_conflicts("com/example/demo.jar")
                .await
                .unwrap(),
            vec![record.path]
        );
    }
}
