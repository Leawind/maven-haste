use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MavenPath {
    relative: String,
    segments: Vec<String>,
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    pub file_type: String,
}

impl MavenPath {
    pub fn parse(uri_path: &str, base_path: &str) -> Result<Self, PathError> {
        let encoded = if base_path == "/" {
            uri_path
                .strip_prefix('/')
                .ok_or(PathError::OutsideBasePath)?
        } else {
            uri_path
                .strip_prefix(base_path)
                .and_then(|suffix| suffix.strip_prefix('/'))
                .ok_or(PathError::OutsideBasePath)?
        };
        if encoded.is_empty() {
            return Err(PathError::Invalid("artifact path is empty".into()));
        }

        let mut segments = Vec::new();
        for encoded_segment in encoded.split('/') {
            if encoded_segment.is_empty() {
                return Err(PathError::Invalid(
                    "artifact path contains an empty segment".into(),
                ));
            }
            let segment = percent_decode_str(encoded_segment)
                .decode_utf8()
                .map_err(|_| PathError::Invalid("artifact path is not valid UTF-8".into()))?
                .into_owned();
            validate_segment(&segment)?;
            segments.push(segment);
        }

        if segments[0].eq_ignore_ascii_case(".maven-haste") {
            return Err(PathError::Invalid(
                "the .maven-haste namespace is reserved".into(),
            ));
        }

        let coordinates = Coordinates::parse(&segments)?;
        Ok(Self {
            relative: segments.join("/"),
            segments,
            group_id: coordinates.group_id,
            artifact_id: coordinates.artifact_id,
            version: coordinates.version,
            file_type: coordinates.file_type,
        })
    }

    pub fn relative(&self) -> &str {
        &self.relative
    }

    pub fn final_path(&self, root: &Path) -> PathBuf {
        self.segments
            .iter()
            .fold(root.to_path_buf(), |path, segment| path.join(segment))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PathError {
    #[error("request path is outside the configured base path")]
    OutsideBasePath,
    #[error("{0}")]
    Invalid(String),
}

struct Coordinates {
    group_id: String,
    artifact_id: String,
    version: String,
    file_type: String,
}

impl Coordinates {
    fn parse(segments: &[String]) -> Result<Self, PathError> {
        let filename = segments
            .last()
            .expect("a Maven path always has at least one segment");
        let metadata =
            filename == "maven-metadata.xml" || filename.starts_with("maven-metadata.xml.");

        if metadata {
            if segments.len() < 2 {
                return Err(PathError::Invalid(
                    "metadata path has no group or artifact component".into(),
                ));
            }
            let before_file = &segments[..segments.len() - 1];
            let version_level = before_file.len() >= 3
                && before_file
                    .last()
                    .is_some_and(|segment| segment.ends_with("-SNAPSHOT"));
            let (group_segments, artifact_id, version) = if version_level {
                (
                    &before_file[..before_file.len() - 2],
                    before_file[before_file.len() - 2].clone(),
                    before_file[before_file.len() - 1].clone(),
                )
            } else if before_file.len() >= 2 {
                (
                    &before_file[..before_file.len() - 1],
                    before_file[before_file.len() - 1].clone(),
                    String::new(),
                )
            } else {
                (before_file, String::new(), String::new())
            };
            return Ok(Self {
                group_id: group_segments.join("."),
                artifact_id,
                version,
                file_type: if filename == "maven-metadata.xml" {
                    "metadata".into()
                } else {
                    checksum_file_type(filename).into()
                },
            });
        }

        if segments.len() < 4 {
            return Err(PathError::Invalid(
                "artifact path must contain group, artifact, version, and filename".into(),
            ));
        }
        let artifact_index = segments.len() - 3;
        Ok(Self {
            group_id: segments[..artifact_index].join("."),
            artifact_id: segments[artifact_index].clone(),
            version: segments[artifact_index + 1].clone(),
            file_type: regular_file_type(filename).into(),
        })
    }
}

fn regular_file_type(filename: &str) -> &'static str {
    if filename.ends_with(".sha1") {
        "sha1"
    } else if filename.ends_with(".sha256") {
        "sha256"
    } else if filename.ends_with(".md5") {
        "md5"
    } else if filename.ends_with(".module") {
        "module"
    } else if filename.ends_with(".pom") {
        "pom"
    } else if filename.ends_with(".jar") {
        "jar"
    } else {
        "artifact"
    }
}

fn checksum_file_type(filename: &str) -> &'static str {
    if filename.ends_with(".sha1") {
        "sha1"
    } else if filename.ends_with(".sha256") {
        "sha256"
    } else if filename.ends_with(".md5") {
        "md5"
    } else {
        "metadata"
    }
}

fn validate_segment(segment: &str) -> Result<(), PathError> {
    if segment.is_empty()
        || matches!(segment, "." | "..")
        || segment.ends_with([' ', '.'])
        || segment.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
        || is_windows_device_name(segment)
    {
        return Err(PathError::Invalid(format!(
            "artifact path contains unsafe segment {segment:?}"
        )));
    }
    Ok(())
}

fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_artifact_coordinates_without_affecting_relative_path() {
        let path =
            MavenPath::parse("/maven/net/fabricmc/loader/1.0/loader-1.0.jar", "/maven").unwrap();
        assert_eq!(path.relative(), "net/fabricmc/loader/1.0/loader-1.0.jar");
        assert_eq!(path.group_id, "net.fabricmc");
        assert_eq!(path.artifact_id, "loader");
        assert_eq!(path.version, "1.0");
        assert_eq!(path.file_type, "jar");
    }

    #[test]
    fn rejects_traversal_encoded_separators_and_internal_namespace() {
        for path in [
            "/maven/com/example/../secret/file.jar",
            "/maven/com/example%2Fsecret/file.jar",
            "/maven/.MAVEN-HASTE/cache.db/x/y",
        ] {
            assert!(MavenPath::parse(path, "/maven").is_err(), "accepted {path}");
        }
    }

    #[test]
    fn parses_snapshot_metadata() {
        let path = MavenPath::parse(
            "/maven/com/example/demo/1.0-SNAPSHOT/maven-metadata.xml",
            "/maven",
        )
        .unwrap();
        assert_eq!(path.group_id, "com.example");
        assert_eq!(path.artifact_id, "demo");
        assert_eq!(path.version, "1.0-SNAPSHOT");
        assert_eq!(path.file_type, "metadata");
    }

    #[test]
    fn supports_root_base_path_and_rejects_double_slashes() {
        let parsed = MavenPath::parse("/g/a/1/a-1.jar", "/").unwrap();
        assert_eq!(parsed.relative(), "g/a/1/a-1.jar");
        assert!(MavenPath::parse("/maven//g/a/1/a-1.jar", "/maven").is_err());
    }
}
