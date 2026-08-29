//! Cache maintenance: prefix removal and integrity verification.

use std::io::ErrorKind;

use crate::cache::io::{hash_file, internal, normalize_cache_prefix, relative_file_path};
use crate::cache::{CacheFailure, CacheManager, IntegrityIssue, IntegrityReport, RemovalStats};

impl CacheManager {
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
            let file_path = relative_file_path(&self.inner.storage.root, &record.path);
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
