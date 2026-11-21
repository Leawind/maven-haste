//! Cache maintenance: capacity sweeps, expired negative-entry cleanup,
//! prefix removal, and integrity verification.

use std::collections::HashSet;
use std::io::ErrorKind;

use crate::cache::io::{
    hash_file, internal, normalize_cache_prefix, recorded_file_path, unix_timestamp,
};
use crate::cache::{CacheFailure, CacheManager, IntegrityIssue, IntegrityReport, RemovalStats};

impl CacheManager {
    /// Runs one maintenance pass: evicts least-recently-used files above the
    /// configured capacity and purges expired negative entries. The server
    /// schedules this periodically so the size limit holds even when no new
    /// files are written.
    pub async fn run_maintenance(&self) -> Result<(), CacheFailure> {
        crate::storage::cleanup_stale_parts(&self.inner.storage.tmp_dir())
            .await
            .map_err(|error| CacheFailure::Internal(error.to_string()))?;
        self.enforce_capacity(&HashSet::new()).await?;
        let ttl = self.inner.config.negative_ttl.as_secs() as i64;
        let cutoff = unix_timestamp().saturating_sub(ttl);
        self.inner
            .database
            .delete_expired_negative_entries(cutoff)
            .await
            .map_err(internal)?;
        self.collect_objects().await
    }

    /// Removes all cached artifacts, checksums, and negative entries under a
    /// path prefix, reporting how much was removed.
    pub async fn remove_prefix(&self, prefix: &str) -> Result<RemovalStats, CacheFailure> {
        let prefix = normalize_cache_prefix(prefix)?;
        let records = self
            .inner
            .database
            .records_with_prefix(&prefix)
            .await
            .map_err(internal)?;
        self.remove_records(records).await
    }

    /// Verifies that every recorded artifact still exists with matching size
    /// and stored hashes, reporting each discrepancy found.
    pub async fn verify(&self) -> Result<IntegrityReport, CacheFailure> {
        let _lifecycle = self.inner.lifecycle.read().await;
        let records = self
            .inner
            .database
            .records_by_access()
            .await
            .map_err(internal)?;
        let mut report = IntegrityReport {
            checked: 0,
            issues: Vec::new(),
        };
        for record in records {
            report.checked += 1;
            let file_path = recorded_file_path(&self.inner.storage.root, &record)
                .await
                .map_err(internal)?;
            match hash_file(&file_path).await {
                Ok((size, sha1, sha256, _)) => {
                    if size != record.file_size.max(0) as u64 {
                        report.issues.push(IntegrityIssue {
                            path: record.path.clone(),
                            reason: format!(
                                "size mismatch: database={}, file={size}",
                                record.file_size.max(0)
                            ),
                        });
                    } else if record
                        .sha256
                        .as_deref()
                        .is_some_and(|expected| expected != sha256)
                    {
                        report.issues.push(IntegrityIssue {
                            path: record.path.clone(),
                            reason: format!(
                                "SHA-256 mismatch: expected {}, found {sha256}",
                                record.sha256.as_deref().expect("checked above")
                            ),
                        });
                    } else if record
                        .sha1
                        .as_deref()
                        .is_some_and(|expected| expected != sha1)
                    {
                        report.issues.push(IntegrityIssue {
                            path: record.path,
                            reason: format!(
                                "SHA-1 mismatch: expected {}, found {sha1}",
                                record.sha1.as_deref().expect("checked above")
                            ),
                        });
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    report.issues.push(IntegrityIssue {
                        path: record.path,
                        reason: "file is missing".into(),
                    });
                }
                Err(error) => {
                    report.issues.push(IntegrityIssue {
                        path: record.path,
                        reason: format!("failed to read file: {error}"),
                    });
                }
            }
        }
        Ok(report)
    }
}
