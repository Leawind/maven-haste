use std::path::PathBuf;

use crate::db::ArtifactRecord;

/// The outcome of a prepared fetch, before anything is written into the cache.
pub(crate) enum PreparedFetch {
    Bundle(Vec<PreparedFile>),
    NotModified,
    NotFound,
    Gateway,
}

/// A fully downloaded artifact that is ready to be atomically installed.
pub(crate) struct PreparedFile {
    pub(crate) relative: String,
    pub(crate) temporary: PathBuf,
    pub(crate) record: ArtifactRecord,
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
