use std::collections::HashSet;
use std::io::ErrorKind;

use tokio::fs;

use crate::cache::io::{
    DigestAlgorithm, cleanup_prepared, hash_file_with, internal, is_timestamp_fresh, lock_objects,
    object_path, recorded_file_path, relative_file_path, remove_empty_parents,
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
        self.install_bundle_checked(files, None).await.map(|_| ())
    }

    pub(crate) async fn install_bundle_checked(
        &self,
        files: Vec<PreparedFile>,
        source: Option<&ArtifactRecord>,
    ) -> Result<bool, CacheFailure> {
        let cache = self.clone();
        let source = source.cloned();
        // Once staging starts, finish the publication even if the requesting
        // HTTP task is canceled. SQLite workers may outlive an awaited future.
        tokio::spawn(async move {
            let result = cache.install_bundle_inner(&files, source.as_ref()).await;
            if result.is_err() {
                cleanup_prepared(&files).await;
            }
            result
        })
        .await
        .map_err(internal)?
    }

    async fn install_bundle_inner(
        &self,
        files: &[PreparedFile],
        source: Option<&ArtifactRecord>,
    ) -> Result<bool, CacheFailure> {
        let lifecycle = self.inner.lifecycle.write().await;
        let publication_lock = lock_objects(&self.inner.storage.root).await?;
        if let Some(source) = source {
            let current = self
                .inner
                .database
                .get(&source.path)
                .await
                .map_err(internal)?;
            if current.is_none_or(|current| current.sha256 != source.sha256) {
                cleanup_prepared(files).await;
                return Ok(false);
            }
        }
        let objects = self.inner.storage.root.join(".maven-haste/objects");
        fs::create_dir_all(&objects).await.map_err(internal)?;
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
        }

        for file in files {
            let hash = file
                .record
                .sha256
                .as_deref()
                .ok_or_else(|| CacheFailure::Internal("prepared file has no SHA-256".into()))?;
            let final_path = object_path(&self.inner.storage.root, hash).ok_or_else(|| {
                CacheFailure::Internal("prepared file has invalid SHA-256".into())
            })?;
            // Existing objects are reusable only if their actual bytes still
            // match. Never overwrite a valid object held by an old response.
            if hash_file_with(&final_path, DigestAlgorithm::Sha256)
                .await
                .is_ok_and(|(size, actual)| {
                    actual == hash && size == file.record.file_size.max(0) as u64
                })
            {
                remove_file_if_exists(&file.temporary)
                    .await
                    .map_err(internal)?;
                continue;
            }
            fs::rename(&file.temporary, &final_path)
                .await
                .map_err(|error| {
                    CacheFailure::Internal(format!(
                        "failed to atomically install {}: {error}",
                        final_path.display()
                    ))
                })?;
        }
        // Failed staging or a crash before this transaction leaves the old
        // bundle intact. Unreferenced objects are collected by maintenance.
        self.inner
            .database
            .publish_bundle(
                files.iter().map(|file| file.record.clone()).collect(),
                conflicts.clone(),
            )
            .await
            .map_err(internal)?;
        for conflict in conflicts {
            if !self.inner.serves.is_busy(&conflict) {
                let path = relative_file_path(&self.inner.storage.root, &conflict);
                if let Err(error) = remove_file_if_exists(&path).await {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "failed to retire a legacy case alias"
                    );
                }
            }
        }
        drop(publication_lock);
        drop(lifecycle);
        let protected = files
            .iter()
            .map(|file| file.relative.as_str())
            .collect::<HashSet<_>>();
        if let Err(error) = self.enforce_capacity(&protected).await {
            tracing::warn!(%error, "cache capacity enforcement did not complete");
        }
        if let Err(error) = self.collect_objects().await {
            tracing::warn!(%error, "object garbage collection did not complete");
        }
        Ok(true)
    }

    pub(crate) async fn enforce_capacity(
        &self,
        protected: &HashSet<&str>,
    ) -> Result<(), CacheFailure> {
        let Some(max_size) = self.runtime().cache.max_size else {
            return Ok(());
        };
        // Recent hits determine the eviction order, so persist them before
        // reading the least recently used records.
        self.flush_hits().await;
        let stats = self.inner.database.stats().await.map_err(internal)?;
        if stats.total_size <= max_size {
            return Ok(());
        }
        let mut remaining = stats.total_size;
        // Read candidates in bounded batches instead of materializing the
        // whole cache. A batch without a single eviction means every remaining
        // candidate is protected or in use, so another query would only repeat
        // the same head records.
        const CAPACITY_BATCH: usize = 512;
        while remaining > max_size {
            let candidates = self
                .inner
                .database
                .records_by_access_limit(CAPACITY_BATCH)
                .await
                .map_err(internal)?;
            if candidates.is_empty() {
                break;
            }
            let mut progressed = false;
            for record in candidates {
                if remaining <= max_size {
                    break;
                }
                if protected.contains(record.path.as_str()) || self.path_is_busy(&record.path) {
                    continue;
                }
                let removed = self.remove_records(vec![record]).await?;
                remaining = remaining.saturating_sub(removed.bytes);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
        Ok(())
    }

    pub(crate) async fn remove_records(
        &self,
        records: Vec<ArtifactRecord>,
    ) -> Result<RemovalStats, CacheFailure> {
        let _lifecycle = self.inner.lifecycle.write().await;
        let _publication_lock = lock_objects(&self.inner.storage.root).await?;
        let mut removed_paths = Vec::new();
        let mut stats = RemovalStats { files: 0, bytes: 0 };
        for record in records {
            let Some(record) = self
                .inner
                .database
                .get(&record.path)
                .await
                .map_err(internal)?
            else {
                continue;
            };
            if self.path_is_busy(&record.path) {
                continue;
            }
            let file_path = recorded_file_path(&self.inner.storage.root, &record)
                .await
                .map_err(internal)?;
            if file_path.starts_with(self.inner.storage.root.join(".maven-haste/objects")) {
                stats.files += 1;
                stats.bytes = stats.bytes.saturating_add(record.file_size.max(0) as u64);
                removed_paths.push(record.path);
                continue;
            }
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
        self.collect_objects_locked().await?;
        Ok(stats)
    }

    pub(crate) async fn collect_objects(&self) -> Result<(), CacheFailure> {
        let _lifecycle = self.inner.lifecycle.write().await;
        let _publication_lock = lock_objects(&self.inner.storage.root).await?;
        self.collect_objects_locked().await
    }

    async fn collect_objects_locked(&self) -> Result<(), CacheFailure> {
        // A reader may still own an older record after a newer bundle was
        // published. Such readers are tracked by object hash, so collection
        // never has to skip an entire pass just because some response is
        // active. Objects younger than the grace period are also kept because
        // another process may be reading them.
        let records = self
            .inner
            .database
            .records_by_access()
            .await
            .map_err(internal)?;
        let retained: HashSet<_> = records
            .iter()
            .filter_map(|record| record.sha256.clone())
            .collect();
        // A legacy Maven-layout file can remain after its record moves to an
        // object. Retire that duplicate only when the replacement is verified.
        for record in &records {
            let legacy = relative_file_path(&self.inner.storage.root, &record.path);
            if fs::try_exists(&legacy).await.map_err(internal)?
                && let Some(object) = record
                    .sha256
                    .as_deref()
                    .and_then(|hash| object_path(&self.inner.storage.root, hash))
                && hash_file_with(&object, DigestAlgorithm::Sha256)
                    .await
                    .is_ok_and(|(size, hash)| {
                        Some(hash.as_str()) == record.sha256.as_deref()
                            && size == record.file_size.max(0) as u64
                    })
            {
                remove_file_if_exists(&legacy).await.map_err(internal)?;
                remove_empty_parents(legacy.parent(), &self.inner.storage.root).await;
            }
        }
        let objects = self.inner.storage.root.join(".maven-haste/objects");
        let mut entries = match fs::read_dir(&objects).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(internal(error)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(internal)? {
            let name = entry.file_name();
            let Some(hash) = name.to_str() else { continue };
            if object_path(&self.inner.storage.root, hash).is_none()
                || retained.contains(hash)
                || self.inner.served_objects.is_busy(hash)
                || !entry.file_type().await.map_err(internal)?.is_file()
            {
                continue;
            }
            let metadata = entry.metadata().await.map_err(internal)?;
            let old_enough = self.inner.object_grace.is_zero()
                || metadata
                    .modified()
                    .ok()
                    .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= self.inner.object_grace);
            if old_enough {
                remove_file_if_exists(&entry.path())
                    .await
                    .map_err(internal)?;
            }
        }
        Ok(())
    }

    fn path_is_busy(&self, path: &str) -> bool {
        self.inner.flights.contains_key(path)
            || self.inner.refreshes.contains_key(path)
            || self.inner.serves.is_busy(path)
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
        let negative_ttl = self.runtime().cache.negative_ttl;
        let (fresh, expired): (Vec<NegativeCacheEntry>, Vec<NegativeCacheEntry>) = entries
            .into_iter()
            .partition(|entry| is_timestamp_fresh(entry.observed_at, negative_ttl));
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
