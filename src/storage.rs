use std::io::ErrorKind;
use std::path::Path;

use tokio::fs::{self, OpenOptions};
use uuid::Uuid;

use crate::config::StorageConfig;
use crate::error::AppError;

#[derive(Clone, Copy, Debug)]
pub struct StorageEnvironment {
    pub case_sensitive: bool,
}

pub async fn prepare(config: &StorageConfig) -> Result<StorageEnvironment, AppError> {
    fs::create_dir_all(&config.root).await.map_err(runtime)?;
    fs::create_dir_all(config.tmp_dir())
        .await
        .map_err(runtime)?;
    let db_parent = config
        .db_path()
        .parent()
        .ok_or_else(|| AppError::Runtime("storage.db_path must have a parent directory".into()))?;
    fs::create_dir_all(db_parent).await.map_err(runtime)?;

    ensure_directory(&config.root).await?;
    ensure_directory(config.tmp_dir()).await?;
    ensure_database_path(config.db_path()).await?;
    probe_atomic_rename(config.tmp_dir(), &config.root).await?;
    let case_sensitive = probe_case_sensitivity(&config.root).await?;
    cleanup_parts(config.tmp_dir()).await?;

    Ok(StorageEnvironment { case_sensitive })
}

async fn ensure_directory(path: &Path) -> Result<(), AppError> {
    let metadata = fs::metadata(path).await.map_err(runtime)?;
    if !metadata.is_dir() {
        return Err(AppError::Runtime(format!(
            "storage path {} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

async fn ensure_database_path(path: &Path) -> Result<(), AppError> {
    match fs::metadata(path).await {
        Ok(metadata) if metadata.is_dir() => Err(AppError::Runtime(format!(
            "storage.db_path {} is a directory",
            path.display()
        ))),
        Ok(_) => OpenOptions::new()
            .write(true)
            .open(path)
            .await
            .map(|_| ())
            .map_err(runtime),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            probe_writable(
                path.parent()
                    .expect("database paths were checked to have a parent"),
            )
            .await
        }
        Err(error) => Err(runtime(error)),
    }
}

async fn probe_writable(directory: &Path) -> Result<(), AppError> {
    let path = directory.join(format!(".maven-haste-write-{}.probe", Uuid::new_v4()));
    create_new(&path).await?;
    remove_if_exists(&path).await
}

async fn probe_atomic_rename(tmp_dir: &Path, root: &Path) -> Result<(), AppError> {
    let id = Uuid::new_v4();
    let source = tmp_dir.join(format!("{id}.probe"));
    let target = root.join(format!(".maven-haste-rename-{id}.probe"));
    create_new(&source).await?;
    if let Err(error) = fs::rename(&source, &target).await {
        let _ = remove_if_exists(&source).await;
        return Err(AppError::Runtime(format!(
            "storage.tmp_dir must support atomic rename into storage.root: {error}"
        )));
    }
    remove_if_exists(&target).await
}

async fn probe_case_sensitivity(root: &Path) -> Result<bool, AppError> {
    let id = Uuid::new_v4().simple().to_string();
    let lower = root.join(format!(".maven-haste-case-{id}.probe"));
    let upper = root.join(format!(".MAVEN-HASTE-CASE-{id}.PROBE"));
    create_new(&lower).await?;

    let upper_result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&upper)
        .await;
    let sensitive = match upper_result {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = remove_if_exists(&lower).await;
            return Err(AppError::Runtime(format!(
                "failed to probe case sensitivity in {}: {error}",
                root.display()
            )));
        }
    };

    remove_if_exists(&lower).await?;
    if sensitive {
        remove_if_exists(&upper).await?;
    }
    Ok(sensitive)
}

async fn create_new(path: &Path) -> Result<(), AppError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map(|_| ())
        .map_err(|error| {
            AppError::Runtime(format!(
                "failed to create storage probe {}: {error}",
                path.display()
            ))
        })
}

async fn cleanup_parts(tmp_dir: &Path) -> Result<(), AppError> {
    let mut entries = fs::read_dir(tmp_dir).await.map_err(runtime)?;
    while let Some(entry) = entries.next_entry().await.map_err(runtime)? {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "part")
            && entry.file_type().await.map_err(runtime)?.is_file()
        {
            fs::remove_file(path).await.map_err(runtime)?;
        }
    }
    Ok(())
}

async fn remove_if_exists(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(runtime(error)),
    }
}

fn runtime(error: std::io::Error) -> AppError {
    AppError::Runtime(error.to_string())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn prepares_directories_and_removes_partial_files() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("repository");
        let config = StorageConfig::resolved_for_test(root.clone());
        fs::create_dir_all(config.tmp_dir()).await.unwrap();
        fs::write(config.tmp_dir().join("abandoned.part"), b"partial")
            .await
            .unwrap();

        let _environment = prepare(&config).await.unwrap();

        assert!(root.is_dir());
        assert!(!config.tmp_dir().join("abandoned.part").exists());
    }

    #[tokio::test]
    async fn rename_atomically_replaces_existing_file() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source.part");
        let destination = directory.path().join("destination.jar");
        fs::write(&source, b"new").await.unwrap();
        fs::write(&destination, b"old").await.unwrap();

        fs::rename(&source, &destination).await.unwrap();

        assert_eq!(fs::read(&destination).await.unwrap(), b"new");
        assert!(!source.exists());
    }
}
