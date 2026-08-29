// `upsert_artifact` generated from `sql/cache.sql` legitimately takes one
// parameter per record column, so the argument-count lint is disabled here.
#![allow(clippy::too_many_arguments)]
use std::path::Path;

use deadpool_sqlite::{Config, Hook, HookError, Pool, Runtime};
use include_sqlite_sql::{impl_sql, include_sql};
use rusqlite;

use crate::error::AppError;

include_sql!("/sql/cache.sql");

/// Connection settings applied to every pooled connection. `journal_mode=WAL`
/// is a persistent database-file property; setting it on each connection keeps
/// fresh databases in WAL mode without mixing pragmas into schema migrations.
const CONNECTION_PRAGMAS: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;
"#;

// Schema migrations embedded from the `migrations/` directory at build time
// (see `build.rs`), which is the single source of truth: adding a migration is
// just adding a `NUM-NAME.sql` file to that directory. Migrations run in
// numeric filename order at startup, each in its own transaction, and
// `PRAGMA user_version` records how many have been applied so pending
// migrations run exactly once.
include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

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
    pub last_accessed: i64,
    pub request_count: i64,
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
            .interact(|connection| run_migrations(connection, MIGRATIONS))
            .await
            .map_err(|error| AppError::Runtime(format!("SQLite worker failed: {error}")))?
            .map_err(|error| AppError::Runtime(format!("failed to migrate SQLite: {error}")))?;
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<Option<ArtifactRecord>, AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            let mut record = None;
            connection.get_artifact(&path, |row| {
                record = Some(artifact_from_row(row)?);
                Ok(())
            })?;
            Ok(record)
        })
        .await
    }

    pub async fn case_conflicts(&self, path: &str) -> Result<Vec<String>, AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            let mut conflicts = Vec::new();
            connection.case_conflicts(&path, |row| {
                conflicts.push(row.get(0)?);
                Ok(())
            })?;
            Ok(conflicts)
        })
        .await
    }

    pub async fn delete_paths(&self, paths: Vec<String>) -> Result<(), AppError> {
        if paths.is_empty() {
            return Ok(());
        }
        self.with_connection(move |connection| {
            let transaction = connection.transaction()?;
            for path in &paths {
                transaction.delete_artifact(path)?;
                transaction.delete_negative_path(path)?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn upsert_many(&self, records: Vec<ArtifactRecord>) -> Result<(), AppError> {
        self.with_connection(move |connection| {
            let transaction = connection.transaction()?;
            for record in records {
                transaction.upsert_artifact(
                    &record.path,
                    &record.group_id,
                    &record.artifact_id,
                    &record.version,
                    &record.file_type,
                    &record.upstream,
                    record.sha1.as_deref(),
                    record.sha256.as_deref(),
                    record.etag.as_deref(),
                    record.last_modified.as_deref(),
                    record.file_size,
                    record.created_at,
                    record.last_refresh_attempt,
                    record.last_accessed,
                )?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn negative_entries(&self, path: &str) -> Result<Vec<NegativeCacheEntry>, AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            let mut entries = Vec::new();
            connection.negative_entries(&path, |row| {
                entries.push(NegativeCacheEntry {
                    repository_id: row.get(0)?,
                    observed_at: row.get(1)?,
                });
                Ok(())
            })?;
            Ok(entries)
        })
        .await
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
        self.with_connection(move |connection| {
            let transaction = connection.transaction()?;
            for repository_id in repository_ids {
                transaction.upsert_negative_entry(&path, &repository_id, observed_at)?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn delete_negative_entry(
        &self,
        path: &str,
        repository_id: &str,
    ) -> Result<(), AppError> {
        let path = path.to_owned();
        let repository_id = repository_id.to_owned();
        self.with_connection(move |connection| {
            connection.delete_negative_entry(&path, &repository_id)?;
            Ok(())
        })
        .await
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
        self.with_connection(move |connection| {
            let transaction = connection.transaction()?;
            for repository_id in repository_ids {
                transaction.delete_negative_entry(&path, &repository_id)?;
            }
            transaction.commit()
        })
        .await
    }

    pub async fn touch_refresh_attempt(&self, path: &str, timestamp: i64) -> Result<(), AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            connection.touch_refresh_attempt(&path, timestamp)?;
            Ok(())
        })
        .await
    }

    pub async fn touch_access(&self, path: &str, timestamp: i64) -> Result<(), AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            connection.touch_access(&path, timestamp)?;
            Ok(())
        })
        .await
    }

    /// Increments the per-artifact request counter for a path.
    pub async fn bump_request_count(&self, path: &str) -> Result<(), AppError> {
        let path = path.to_owned();
        self.with_connection(move |connection| {
            connection.bump_request_count(&path)?;
            Ok(())
        })
        .await
    }

    pub async fn records_by_access(&self) -> Result<Vec<ArtifactRecord>, AppError> {
        self.with_connection(|connection| {
            let mut records = Vec::new();
            connection.records_by_access(|row| {
                records.push(artifact_from_row(row)?);
                Ok(())
            })?;
            Ok(records)
        })
        .await
    }

    pub async fn records_with_prefix(&self, prefix: &str) -> Result<Vec<ArtifactRecord>, AppError> {
        let prefix = prefix.trim_matches('/').to_owned();
        let descendant = format!("{}/%", escape_like(&prefix));
        self.with_connection(move |connection| {
            let mut records = Vec::new();
            connection.records_with_prefix(&prefix, &descendant, |row| {
                records.push(artifact_from_row(row)?);
                Ok(())
            })?;
            Ok(records)
        })
        .await
    }

    pub async fn ping(&self) -> Result<(), AppError> {
        self.with_connection(|connection| {
            connection.ping(|_| Ok(()))?;
            Ok(())
        })
        .await
    }

    pub async fn stats(&self) -> Result<DatabaseStats, AppError> {
        self.with_connection(|connection| {
            let mut stats = DatabaseStats {
                files: 0,
                total_size: 0,
                negative_entries: 0,
            };
            connection.stats(|row| {
                stats = DatabaseStats {
                    files: row.get::<_, u64>(0)?,
                    total_size: row.get::<_, u64>(1)?,
                    negative_entries: row.get::<_, u64>(2)?,
                };
                Ok(())
            })?;
            Ok(stats)
        })
        .await
    }

    async fn connection(&self) -> Result<deadpool_sqlite::Connection, AppError> {
        self.pool
            .get()
            .await
            .map_err(|error| AppError::Runtime(format!("failed to get SQLite connection: {error}")))
    }

    async fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut rusqlite::Connection) -> Result<T, rusqlite::Error> + Send + 'static,
    ) -> Result<T, AppError>
    where
        T: Send + 'static,
    {
        let connection = self.connection().await?;
        connection
            .interact(operation)
            .await
            .map_err(worker_error)?
            .map_err(sqlite_error)
    }
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> Result<ArtifactRecord, rusqlite::Error> {
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
        last_accessed: row.get(13)?,
        request_count: row.get(14)?,
    })
}

fn run_migrations(
    connection: &mut rusqlite::Connection,
    migrations: &[&str],
) -> Result<(), rusqlite::Error> {
    let applied: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let from = applied.max(0);
    let target = migrations.len() as i64;
    if from < target {
        tracing::info!(from, to = target, "migrating database schema");
    }
    for (index, source) in migrations.iter().enumerate().skip(from as usize) {
        let transaction = connection.transaction()?;
        transaction.execute_batch(source)?;
        transaction.pragma_update(None, "user_version", (index + 1) as i64)?;
        transaction.commit()?;
    }
    Ok(())
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn worker_error(error: deadpool_sqlite::InteractError) -> AppError {
    AppError::Runtime(format!("SQLite worker failed: {error}"))
}

fn sqlite_error(error: rusqlite::Error) -> AppError {
    AppError::Runtime(format!("SQLite operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn opens_an_empty_database() {
        let directory = TempDir::new().unwrap();
        let database = Database::open(&directory.path().join("cache.db"))
            .await
            .unwrap();
        assert_eq!(
            database.stats().await.unwrap(),
            DatabaseStats {
                files: 0,
                total_size: 0,
                negative_entries: 0,
            }
        );
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
            let (journal_mode, synchronous, busy_timeout) = connection
                .interact(|connection| {
                    Ok::<_, rusqlite::Error>((
                        connection
                            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?,
                        connection
                            .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))?,
                        connection
                            .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))?,
                    ))
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(journal_mode, "wal");
            assert_eq!(synchronous, 1);
            assert_eq!(busy_timeout, 5_000);
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
            last_accessed: 123,
            request_count: 0,
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

    #[tokio::test]
    async fn counts_requests_per_path_and_preserves_them_across_upserts() {
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
            etag: None,
            last_modified: None,
            file_size: 42,
            created_at: 123,
            last_refresh_attempt: None,
            last_accessed: 123,
            request_count: 0,
        };
        database.upsert_many(vec![record.clone()]).await.unwrap();
        assert_eq!(
            database
                .get(&record.path)
                .await
                .unwrap()
                .unwrap()
                .request_count,
            0
        );

        database.bump_request_count(&record.path).await.unwrap();
        database.bump_request_count(&record.path).await.unwrap();
        database.upsert_many(vec![record.clone()]).await.unwrap();
        assert_eq!(
            database
                .get(&record.path)
                .await
                .unwrap()
                .unwrap()
                .request_count,
            2
        );

        database
            .bump_request_count("missing/path.jar")
            .await
            .unwrap();
        assert!(database.get("missing/path.jar").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn persists_negative_entries_per_path_and_repository() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cache.db");
        let database = Database::open(&path).await.unwrap();
        database
            .upsert_negative_entries(
                "com/example/maven-metadata.xml",
                vec!["repo-a".into(), "repo-b".into()],
                123,
            )
            .await
            .unwrap();
        database
            .upsert_negative_entries("com/example/maven-metadata.xml", vec!["repo-a".into()], 456)
            .await
            .unwrap();
        drop(database);

        let database = Database::open(&path).await.unwrap();
        let mut entries = database
            .negative_entries("com/example/maven-metadata.xml")
            .await
            .unwrap();
        entries.sort_by(|left, right| left.repository_id.cmp(&right.repository_id));
        assert_eq!(
            entries,
            vec![
                NegativeCacheEntry {
                    repository_id: "repo-a".into(),
                    observed_at: 456,
                },
                NegativeCacheEntry {
                    repository_id: "repo-b".into(),
                    observed_at: 123,
                },
            ]
        );
        assert_eq!(database.stats().await.unwrap().negative_entries, 2);

        database
            .delete_negative_entry("com/example/maven-metadata.xml", "repo-a")
            .await
            .unwrap();
        assert_eq!(database.stats().await.unwrap().negative_entries, 1);
    }

    #[test]
    fn applies_pending_migrations_in_order_and_skips_applied() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        let migrations = [
            "CREATE TABLE first (id INTEGER PRIMARY KEY);",
            "ALTER TABLE first ADD COLUMN extra TEXT;",
        ];
        run_migrations(&mut connection, &migrations).unwrap();
        assert_eq!(user_version_for(&connection), 2);
        connection
            .execute("INSERT INTO first (id, extra) VALUES (1, 'x')", [])
            .unwrap();

        run_migrations(&mut connection, &migrations).unwrap();
        assert_eq!(user_version_for(&connection), 2);
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM first", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn failed_migration_keeps_previous_version_and_rolls_back() {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        let first = ["CREATE TABLE first (id INTEGER PRIMARY KEY);"];
        run_migrations(&mut connection, &first).unwrap();
        assert_eq!(user_version_for(&connection), 1);

        let broken = [
            "ALTER TABLE first ADD COLUMN extra TEXT;",
            "THIS IS NOT VALID SQL;",
        ];
        assert!(run_migrations(&mut connection, &broken).is_err());
        assert_eq!(user_version_for(&connection), 1);

        let columns: Vec<String> = {
            let mut statement = connection.prepare("PRAGMA table_info(first)").unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(columns, vec!["id"]);
    }

    #[test]
    fn embeds_migrations_in_numeric_order() {
        // The migration list is generated from the `migrations/` directory by
        // build.rs, so this test guards against ordering regressions there.
        assert_eq!(MIGRATIONS.len(), 2);
        assert!(MIGRATIONS[0].contains("CREATE TABLE IF NOT EXISTS artifacts"));
        assert!(MIGRATIONS[1].contains("request_count"));
    }

    fn user_version_for(connection: &rusqlite::Connection) -> i64 {
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn fresh_database_ends_at_the_latest_schema_version() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cache.db");
        let database = Database::open(&path).await.unwrap();
        let connection = database.pool.get().await.unwrap();
        let version = connection
            .interact(|connection| {
                connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);

        let columns = connection
            .interact(|connection| {
                let mut statement = connection.prepare("PRAGMA table_info(artifacts)")?;
                statement
                    .query_map([], |row| row.get::<_, String>(1))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .unwrap()
            .unwrap();
        assert!(columns.iter().any(|column| column == "last_accessed"));
        assert!(columns.iter().any(|column| column == "request_count"));
    }

    #[tokio::test]
    async fn adopts_an_existing_database_without_changing_data() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cache.db");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch(MIGRATIONS[0]).unwrap();
            connection
                .execute(
                    "INSERT INTO artifacts (path, group_id, artifact_id, version, file_type, \
                     upstream, file_size, created_at, last_accessed) \
                     VALUES ('Com/Example/demo.jar', 'Com.Example', 'demo', '1.0', 'jar', \
                     'central', 42, 123, 123)",
                    [],
                )
                .unwrap();
        }

        let database = Database::open(&path).await.unwrap();
        assert_eq!(database.stats().await.unwrap().files, 1);
        let adopted = database.get("Com/Example/demo.jar").await.unwrap().unwrap();
        assert_eq!(adopted.file_size, 42);
        assert_eq!(adopted.request_count, 0);

        let connection = database.pool.get().await.unwrap();
        let version = connection
            .interact(|connection| {
                connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }
}
