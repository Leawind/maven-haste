use std::path::Path;

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
}

#[cfg(test)]
mod tests {
    use deadpool_sqlite::rusqlite::OptionalExtension;
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
}
