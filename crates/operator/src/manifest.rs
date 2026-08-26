//! Release-publish manifest file: the JSON input form of `release publish`.
//!
//! Chosen input format (flag-column rejected: five kinds × three fields do not fit a command
//! line readably):
//!
//! ```json
//! {
//!   "release_id": "release-2026-08-26-a",
//!   "artifacts": [
//!     { "kind": "Router",         "version": "1.0.0", "sha256": "<64 hex chars>" },
//!     { "kind": "Graph",          "version": "1.0.0", "sha256": "<64 hex chars>" },
//!     { "kind": "PropertyIndex",  "version": "1.0.0", "sha256": "<64 hex chars>" },
//!     { "kind": "VectorCanister", "version": "1.0.0", "sha256": "<64 hex chars>" },
//!     { "kind": "TextCanister",   "version": "1.0.0", "sha256": "<64 hex chars>" }
//!   ]
//! }
//! ```
//!
//! Exactly one artifact per non-Provision kind is required, matching Provision's manifest
//! invariant (`IncompleteManifest` / `NotUniquePerKind`); validating locally gives fast
//! feedback without spending an update call.

use std::path::Path;

use gleaph_artifact_api::types::{ArtifactId, ReleaseId, ReleasePublishArgs};
use serde::Deserialize;
use thiserror::Error;

use crate::encoding::{kind_name, parse_sha256_hex};

/// A release-manifest file on disk.
#[derive(Debug, Deserialize)]
struct ManifestFile {
    release_id: String,
    artifacts: Vec<ManifestArtifact>,
}

/// One declared artifact entry.
#[derive(Debug, Deserialize)]
struct ManifestArtifact {
    kind: String,
    version: String,
    sha256: String,
}

/// Manifest loading failures.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("read manifest {}: {detail}", path.display())]
    Read {
        /// Manifest path.
        path: std::path::PathBuf,
        /// Underlying IO error text.
        detail: String,
    },
    /// The file is not valid JSON of the expected shape.
    #[error("parse manifest: {0}")]
    Parse(String),
    /// The parsed manifest violates the one-artifact-per-kind contract.
    #[error("{0}")]
    Invalid(String),
}

/// Load and validate a release manifest file into `release_publish` arguments.
pub fn load_release_manifest(path: &Path) -> Result<ReleasePublishArgs, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|error| ManifestError::Read {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let file: ManifestFile =
        serde_json::from_str(&text).map_err(|error| ManifestError::Parse(error.to_string()))?;
    validate(file.release_id, &file.artifacts)
        .map_err(|reason| ManifestError::Invalid(format!("{}: {}", path.display(), reason)))
}

fn validate(
    release_id: String,
    artifacts: &[ManifestArtifact],
) -> Result<ReleasePublishArgs, String> {
    if release_id.trim().is_empty() {
        return Err("release_id must be a non-empty string".to_owned());
    }
    let mut ids = Vec::with_capacity(artifacts.len());
    let mut seen: Vec<(gleaph_artifact_api::types::CanisterKind, &str)> = Vec::new();
    for entry in artifacts {
        let kind = crate::encoding::parse_kind(&entry.kind)
            .map_err(|error| format!("artifact entry: {error}"))?;
        if let Some((_, first_version)) = seen.iter().find(|(seen_kind, _)| *seen_kind == kind) {
            return Err(format!(
                "two entries for kind {}; versions {first_version:?} and {:?}",
                kind_name(kind),
                entry.version
            ));
        }
        if entry.version.trim().is_empty() {
            return Err(format!(
                "artifact entry for {} has an empty version",
                entry.kind
            ));
        }
        let sha256 = parse_sha256_hex(&entry.sha256)
            .map_err(|error| format!("artifact entry for {}: {error}", entry.kind))?;
        seen.push((kind, entry.version.as_str()));
        ids.push(ArtifactId::new(kind, entry.version.clone(), sha256));
    }
    // Completeness: every non-Provision kind must appear exactly once.
    for expected in [
        gleaph_artifact_api::types::CanisterKind::Router,
        gleaph_artifact_api::types::CanisterKind::Graph,
        gleaph_artifact_api::types::CanisterKind::PropertyIndex,
        gleaph_artifact_api::types::CanisterKind::VectorCanister,
        gleaph_artifact_api::types::CanisterKind::TextCanister,
    ] {
        if !seen.iter().any(|(kind, _)| *kind == expected) {
            return Err(format!(
                "manifest is missing the {} artifact",
                kind_name(expected)
            ));
        }
    }
    Ok(ReleasePublishArgs {
        artifact_ids: ids,
        release_id: ReleaseId(release_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_artifact_api::types::CanisterKind;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    const SHA_A: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA_B: &str = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";

    fn write_manifest(entries: &str) -> std::path::PathBuf {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "operator-manifest-test-{}-{nonce}.json",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(r#"{{"release_id":"rel-1","artifacts":[{entries}]}}"#),
        )
        .expect("write manifest");
        path
    }

    fn entry(kind: &str, sha: &str) -> String {
        format!(r#"{{"kind":"{kind}","version":"1.0.0","sha256":"{sha}"}}"#)
    }

    fn all_five() -> String {
        [
            entry("Router", SHA_A),
            entry("Graph", SHA_B),
            entry("PropertyIndex", SHA_A),
            entry("VectorCanister", SHA_B),
            entry("TextCanister", SHA_A),
        ]
        .join(",")
    }

    #[test]
    fn complete_manifest_loads_into_publish_args() {
        let args = load_release_manifest(&write_manifest(&all_five())).expect("load");
        assert_eq!(args.release_id, ReleaseId("rel-1".to_owned()));
        assert_eq!(args.artifact_ids.len(), 5);
        let router = args
            .artifact_ids
            .iter()
            .find(|id| id.canister_kind == CanisterKind::Router)
            .expect("router");
        assert_eq!(router.semantic_version, "1.0.0");
        assert_eq!(router.sha256[0], 0xba);
    }

    #[test]
    fn duplicate_and_missing_kinds_are_rejected_locally() {
        let duplicate = format!("{},{}", entry("Router", SHA_A), entry("Router", SHA_B));
        let error = load_release_manifest(&write_manifest(&duplicate))
            .expect_err("duplicate kind")
            .to_string();
        assert!(
            error.contains("two entries for kind Router"),
            "got: {error}"
        );

        let missing = [
            entry("Router", SHA_A),
            entry("Graph", SHA_B),
            entry("PropertyIndex", SHA_A),
            entry("VectorCanister", SHA_B),
        ]
        .join(",");
        let error = load_release_manifest(&write_manifest(&missing))
            .expect_err("missing kind")
            .to_string();
        assert!(
            error.contains("missing the TextCanister artifact"),
            "got: {error}"
        );
    }

    #[test]
    fn malformed_entries_are_rejected_with_context() {
        let bad_sha = entry("Router", "nothex");
        let error = load_release_manifest(&write_manifest(&bad_sha)).expect_err("bad sha");
        assert!(
            error.to_string().contains("sha256 must be exactly"),
            "got: {error}"
        );

        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let bad_json_path = std::env::temp_dir().join(format!(
            "operator-manifest-bad-{}-{nonce}.json",
            std::process::id()
        ));
        std::fs::write(&bad_json_path, "{ not json").expect("write");
        let error = load_release_manifest(&bad_json_path).expect_err("bad json");
        assert!(matches!(error, ManifestError::Parse(_)));

        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "operator-manifest-empty-{}-{nonce}.json",
            std::process::id()
        ));
        std::fs::write(&path, r#"{"release_id":"","artifacts":[]}"#).expect("write");
        let error = load_release_manifest(&path).expect_err("empty release id");
        assert!(error.to_string().contains("non-empty"), "got: {error}");
    }
}
