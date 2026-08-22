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
    last_refresh_attempt INTEGER,
    is_not_found INTEGER NOT NULL DEFAULT 0
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
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub file_size: i64,
    pub created_at: i64,
    pub last_refresh_attempt: Option<i64>,
    pub is_not_found: bool,
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
                        "SELECT path, group_id, artifact_id, version, file_type, upstream, etag, \
                         last_modified, file_size, created_at, last_refresh_attempt, is_not_found \
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
                                etag: row.get(6)?,
                                last_modified: row.get(7)?,
                                file_size: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                                created_at: row.get(9)?,
                                last_refresh_attempt: row.get(10)?,
                                is_not_found: row.get::<_, i64>(11)? != 0,
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
                    transaction.execute("DELETE FROM artifacts WHERE path = ?1", [path])?;
                }
                transaction.commit()
            })
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }

    pub async fn upsert(&self, record: ArtifactRecord) -> Result<(), AppError> {
        let connection = self.connection().await?;
        connection
            .interact(move |connection| {
                connection.execute(
                    "INSERT INTO artifacts (path, group_id, artifact_id, version, file_type, \
                     upstream, etag, last_modified, file_size, created_at, last_refresh_attempt, \
                     is_not_found) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                     ON CONFLICT(path) DO UPDATE SET group_id = excluded.group_id, \
                     artifact_id = excluded.artifact_id, version = excluded.version, \
                     file_type = excluded.file_type, upstream = excluded.upstream, \
                     etag = excluded.etag, last_modified = excluded.last_modified, \
                     file_size = excluded.file_size, created_at = excluded.created_at, \
                     last_refresh_attempt = excluded.last_refresh_attempt, \
                     is_not_found = excluded.is_not_found",
                    params![
                        record.path,
                        record.group_id,
                        record.artifact_id,
                        record.version,
                        record.file_type,
                        record.upstream,
                        record.etag,
                        record.last_modified,
                        record.file_size,
                        record.created_at,
                        record.last_refresh_attempt,
                        i64::from(record.is_not_found),
                    ],
                )?;
                Ok(())
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
            etag: Some("tag".into()),
            last_modified: None,
            file_size: 42,
            created_at: 123,
            last_refresh_attempt: None,
            is_not_found: false,
        };
        database.upsert(record.clone()).await.unwrap();

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
