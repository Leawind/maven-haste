use std::collections::HashSet;
use std::io::ErrorKind;

use tokio::fs;

use crate::cache::io::{
    cleanup_prepared, internal, is_timestamp_fresh, relative_file_path, remove_empty_parents,
    remove_file_if_exists,
};
use crate::cache::types::PreparedFile;
use crate::cache::{CacheFailure, RemovalStats};
use crate::db::{ArtifactRecord, NegativeCacheEntry};

impl crate::cache::CacheManager {
    pub(crate) async fn install_bundle(
        &self,
        files: Vec<PreparedFile>,
    ) -> Result<(), CacheFailure> {
        let result = self.install_bundle_inner(&files).await;
        if result.is_err() {
            cleanup_prepared(&files).await;
        }
        result
    }

    async fn install_bundle_inner(&self, files: &[PreparedFile]) -> Result<(), CacheFailure> {
        let mut conflicts = Vec::new();
        if !self.inner.case_sensitive {
            for file in files {
                conflicts.extend(
                    self.inner
                        .database
                        .case_conflicts(&file.relative)
                        .await
                        .map_err(internal)?,
                );
            }
            conflicts.sort();
            conflicts.dedup();
            self.inner
                .database
                .delete_paths(conflicts.clone())
                .await
                .map_err(internal)?;
            for conflict in conflicts {
                remove_file_if_exists(&relative_file_path(&self.inner.storage.root, &conflict))
                    .await
                    .map_err(|error| {
                        CacheFailure::Internal(format!(
                            "failed to remove case-conflicting file {conflict}: {error}"
                        ))
                    })?;
            }
        }

        for file in files {
            let final_path = relative_file_path(&self.inner.storage.root, &file.relative);
            let parent = final_path
                .parent()
                .ok_or_else(|| CacheFailure::Internal("artifact path has no parent".into()))?;
            fs::create_dir_all(parent).await.map_err(|error| {
                CacheFailure::Internal(format!(
                    "failed to create cache directory {}: {error}",
                    parent.display()
                ))
            })?;
        }

        let install_order = (1..files.len()).chain(std::iter::once(0));
        for index in install_order {
            let file = &files[index];
            let final_path = relative_file_path(&self.inner.storage.root, &file.relative);
            fs::rename(&file.temporary, &final_path)
                .await
                .map_err(|error| {
                    CacheFailure::Internal(format!(
                        "failed to atomically install {}: {error}",
                        final_path.display()
                    ))
                })?;
        }
        self.inner
            .database
            .upsert_many(files.iter().map(|file| file.record.clone()).collect())
            .await
            .map_err(internal)?;
        let protected = files
            .iter()
            .map(|file| file.relative.as_str())
            .collect::<HashSet<_>>();
        if let Err(error) = self.enforce_capacity(&protected).await {
            tracing::warn!(%error, "cache capacity enforcement did not complete");
        }
        Ok(())
    }

    pub(crate) async fn enforce_capacity(
        &self,
        protected: &HashSet<&str>,
    ) -> Result<(), CacheFailure> {
        let Some(max_size) = self.inner.config.max_size else {
            return Ok(());
        };
        let stats = self.inner.database.stats().await.map_err(internal)?;
        if stats.total_size <= max_size {
            return Ok(());
        }
        let mut remaining = stats.total_size;
        let candidates = self
            .inner
            .database
            .records_by_access()
            .await
            .map_err(internal)?;
        for record in candidates {
            if remaining <= max_size {
                break;
            }
            if protected.contains(record.path.as_str()) || self.path_is_busy(&record.path) {
                continue;
            }
            let removed = self.remove_records(vec![record]).await?;
            remaining = remaining.saturating_sub(removed.bytes);
        }
        Ok(())
    }

    pub(crate) async fn remove_records(
        &self,
        records: Vec<ArtifactRecord>,
    ) -> Result<RemovalStats, CacheFailure> {
        let mut removed_paths = Vec::new();
        let mut stats = RemovalStats { files: 0, bytes: 0 };
        for record in records {
            if self.path_is_busy(&record.path) {
                continue;
            }
            let file_path = relative_file_path(&self.inner.storage.root, &record.path);
            match fs::remove_file(&file_path).await {
                Ok(()) => {
                    stats.files += 1;
                    stats.bytes = stats.bytes.saturating_add(record.file_size.max(0) as u64);
                    removed_paths.push(record.path);
                    remove_empty_parents(file_path.parent(), &self.inner.storage.root).await;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    stats.bytes = stats.bytes.saturating_add(record.file_size.max(0) as u64);
                    removed_paths.push(record.path);
                }
                Err(error) => {
                    tracing::warn!(path = %file_path.display(), %error, "failed to remove cached file");
                }
            }
        }
        self.inner
            .database
            .delete_paths(removed_paths)
            .await
            .map_err(internal)?;
        Ok(stats)
    }

    fn path_is_busy(&self, path: &str) -> bool {
        self.inner.flights.contains_key(path) || self.inner.refreshes.contains_key(path)
    }

    pub(crate) async fn fresh_negative_entries(
        &self,
        path: &str,
    ) -> Result<HashSet<String>, CacheFailure> {
        let entries = self
            .inner
            .database
            .negative_entries(path)
            .await
            .map_err(internal)?;
        let (fresh, expired): (Vec<NegativeCacheEntry>, Vec<NegativeCacheEntry>) =
            entries.into_iter().partition(|entry| {
                is_timestamp_fresh(entry.observed_at, self.inner.config.negative_ttl)
            });
        self.inner
            .database
            .delete_negative_entries(
                path,
                expired
                    .into_iter()
                    .map(|entry| entry.repository_id)
                    .collect(),
            )
            .await
            .map_err(internal)?;
        Ok(fresh.into_iter().map(|entry| entry.repository_id).collect())
    }
}
