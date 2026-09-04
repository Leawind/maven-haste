use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;

use crate::db::ArtifactRecord;

/// Tracks which cached files are currently being served. Eviction and removal
/// consult the registry so a response never loses its file mid-stream.
#[derive(Default)]
pub(crate) struct ServeRegistry {
    active: Arc<DashMap<String, usize>>,
}

impl ServeRegistry {
    /// Marks a cached file as being served; the returned guard releases the
    /// mark when dropped.
    pub(crate) fn acquire(&self, path: &str) -> ServeGuard {
        *self.active.entry(path.to_owned()).or_default() += 1;
        ServeGuard {
            registry: Arc::clone(&self.active),
            path: path.to_owned(),
        }
    }

    pub(crate) fn is_busy(&self, path: &str) -> bool {
        self.active.contains_key(path)
    }
}

/// An active-serve mark for one cached file.
pub struct ServeGuard {
    registry: Arc<DashMap<String, usize>>,
    path: String,
}

impl std::fmt::Debug for ServeGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServeGuard")
            .field("path", &self.path)
            .finish()
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        if let Some(mut count) = self.registry.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                drop(count);
                self.registry.remove(&self.path);
            }
        }
    }
}

/// The outcome of a prepared fetch, before anything is written into the cache.
pub(crate) enum PreparedFetch {
    Bundle(Vec<PreparedFile>),
    NotModified,
    NotFound,
    Gateway,
}

/// A fully downloaded artifact that is ready to be atomically installed.
#[derive(Clone)]
pub(crate) struct PreparedFile {
    pub(crate) relative: String,
    pub(crate) temporary: PathBuf,
    pub(crate) record: ArtifactRecord,
}

/// Whether a download was written into the cache or must be served directly.
#[derive(Clone)]
pub(crate) enum DownloadOutcome {
    Installed,
    Passthrough(Box<PassthroughFile>),
}

/// A downloaded file served directly to responses without being installed in
/// the cache. All concurrent requests that share one download flight hold a
/// reference to the same temporary file; the file is removed only when the
/// last reference is dropped, so one response finishing can never remove the
/// file out from under another.
#[derive(Clone, Debug)]
pub(crate) struct PassthroughFile {
    pub(crate) temporary: SharedTemp,
    pub(crate) record: ArtifactRecord,
}

/// A temporary file whose removal is tied to shared ownership: cloning adds a
/// reference, dropping the last reference removes the file.
pub struct SharedTemp {
    inner: Arc<SharedTempInner>,
}

struct SharedTempInner {
    path: PathBuf,
    references: AtomicUsize,
}

impl SharedTemp {
    /// Takes ownership of a temporary file with a single reference.
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(SharedTempInner {
                path,
                references: AtomicUsize::new(1),
            }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }
}

impl Clone for SharedTemp {
    fn clone(&self) -> Self {
        self.inner.references.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for SharedTemp {
    fn drop(&mut self) {
        if self.inner.references.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        match std::fs::remove_file(&self.inner.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::debug!(
                path = %self.inner.path.display(),
                %error,
                "failed to remove shared temporary file"
            ),
        }
    }
}

impl std::fmt::Debug for SharedTemp {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedTemp")
            .field("path", &self.inner.path)
            .finish()
    }
}

/// A downloaded main artifact plus its computed hashes and validators.
pub(crate) struct DownloadedMain {
    pub(crate) temporary: PathBuf,
    pub(crate) size: u64,
    pub(crate) sha1: String,
    pub(crate) sha256: String,
    pub(crate) sha512: String,
    pub(crate) etag: Option<String>,
    pub(crate) last_modified: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_the_file_only_after_the_last_reference_is_dropped() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("shared.part");
        std::fs::write(&path, b"content").unwrap();

        let original = SharedTemp::new(path.clone());
        let first_clone = original.clone();
        let second_clone = original.clone();

        drop(first_clone);
        assert!(path.exists());
        drop(original);
        assert!(path.exists());
        drop(second_clone);
        assert!(!path.exists());
    }

    #[test]
    fn dropping_a_reference_for_a_missing_file_is_not_an_error() {
        let original = SharedTemp::new(std::env::temp_dir().join("maven-haste-missing.part"));
        let clone = original.clone();
        std::fs::remove_file(original.path()).ok();
        drop(clone);
        drop(original);
    }
}
