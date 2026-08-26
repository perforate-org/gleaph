//! Operator-facing error type with human-readable rendering for every server rejection.
//!
//! `gleaph-artifact-api` deliberately ships its wire-mirror error enums without `Display`
//! (a neutral contract crate must not pick presentation). This module owns the operator
//! presentation layer: exhaustive renderers for [`IngestError`], [`ArtifactError`],
//! [`ReleaseError`], [`InstallError`], and [`AdminInstallError`], plus the top-level
//! [`OperatorError`] that commands return. Every renderer matches exhaustively so a new
//! server variant forces a rendering decision here instead of silently degrading output.

use std::fmt::Write as _;

use gleaph_artifact_api::driver::IngestError;
use gleaph_artifact_api::types::{ArtifactError, ReleaseError};
use thiserror::Error;

use crate::encoding::{artifact_id_label, kind_name, to_hex};
use crate::manifest::ManifestError;
use crate::wire::{AdminInstallError, InstallError};

/// Everything that can abort an operator command.
///
/// The large wire-mirror error payloads are boxed to keep the error type small; the wire
/// types themselves stay unboxed everywhere else.
#[derive(Debug, Error)]
pub enum OperatorError {
    /// Argument parsing, file loading, or connection setup failure.
    #[error("{0}")]
    Message(String),
    /// The IC ingress layer failed before the canister could answer.
    #[error(transparent)]
    Ingress(#[from] crate::transport::IngressError),
    /// The ingestion driver reported a terminal failure.
    #[error("ingestion failed: {}", describe_ingest_error(&.0))]
    Ingest(Box<IngestError>),
    /// An artifact-catalog call was rejected by Provision.
    #[error("artifact catalog rejected the operation: {}", describe_artifact_error(&.0))]
    ArtifactCatalog(Box<ArtifactError>),
    /// A release call was rejected by Provision.
    #[error("release rejected the operation: {}", describe_release_error(&.0))]
    ReleaseCatalog(Box<ReleaseError>),
    /// `release_install` failed.
    #[error("release install failed: {}", describe_install_error(&.0))]
    Install(Box<InstallError>),
    /// `admin_install_deployment_binding` failed.
    #[error("binding install failed: {}", describe_admin_install_error(&.0))]
    Binding(Box<AdminInstallError>),
    /// The release manifest file is malformed or incomplete.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

impl From<IngestError> for OperatorError {
    fn from(error: IngestError) -> Self {
        Self::Ingest(Box::new(error))
    }
}

impl From<String> for OperatorError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<ArtifactError> for OperatorError {
    fn from(error: ArtifactError) -> Self {
        Self::ArtifactCatalog(Box::new(error))
    }
}

impl From<ReleaseError> for OperatorError {
    fn from(error: ReleaseError) -> Self {
        Self::ReleaseCatalog(Box::new(error))
    }
}

impl From<InstallError> for OperatorError {
    fn from(error: InstallError) -> Self {
        Self::Install(Box::new(error))
    }
}

impl From<AdminInstallError> for OperatorError {
    fn from(error: AdminInstallError) -> Self {
        Self::Binding(Box::new(error))
    }
}

/// Render a driver failure. `Server` delegates to the artifact renderer so every catalog
/// rejection has exactly one presentation.
pub fn describe_ingest_error(error: &IngestError) -> String {
    match error {
        IngestError::Server(inner) => describe_artifact_error(inner),
        IngestError::UploadFailed { reason } => {
            format!("upload is terminally failed on the server: {reason}")
        }
    }
}

/// Render an artifact-catalog rejection.
#[allow(clippy::too_many_lines)] // one arm per wire variant keeps the mapping auditable
pub fn describe_artifact_error(error: &ArtifactError) -> String {
    match error {
        ArtifactError::TooManyChunks { max, declared } => format!(
            "chunk count violates the bound (declared {declared}, max {max}); \
             republish with fewer/larger chunks only if a new identity is intended"
        ),
        ArtifactError::NotProvision(kind) => format!(
            "kind {} is forbidden — Provision self-upgrade is excluded",
            kind_name(*kind)
        ),
        ArtifactError::UnknownArtifact(id) => {
            format!("no metadata exists under this identity ({})", artifact_id_label(id))
        }
        ArtifactError::Unauthorized => {
            "caller is not the resolved governance authority; pass --identity with the governance PEM"
                .to_owned()
        }
        ArtifactError::ChunkHashMismatch {
            chunk_index,
            artifact_id,
        } => format!(
            "chunk {chunk_index} does not hash to its declared value ({})",
            artifact_id_label(artifact_id)
        ),
        ArtifactError::ChunkOutOfRange {
            chunk_index,
            declared,
            ..
        } => format!(
            "chunk index {chunk_index} lies outside the declared range of {declared} chunks"
        ),
        ArtifactError::ConflictingMetadata { requested, existing } => {
            let mut text = format!(
                "metadata already exists under a conflicting identity:\n  requested: {}\n  existing:  {}",
                artifact_id_label(requested),
                artifact_id_label(existing)
            );
            if requested == existing {
                // Identical re-publishes are expected during idempotent resume; tools above
                // the driver normally absorb this signal.
                let _ = write!(text, "\n  (identical re-publish; safe to continue)");
            }
            text
        }
        ArtifactError::FullSha256Mismatch {
            actual,
            artifact_id,
            expected,
        } => format!(
            "full-artifact SHA-256 mismatch for {}:\n  expected: {}\n  actual:   {}",
            artifact_id_label(artifact_id),
            to_hex(expected),
            to_hex(actual)
        ),
        ArtifactError::ArtifactTooLarge { max, byte_length } => format!(
            "artifact byte length exceeds the bound ({byte_length} > {max})"
        ),
        ArtifactError::SemanticVersionTooLong { max } => format!(
            "semantic version string exceeds the length bound (max {max})"
        ),
    }
}

/// Render a release rejection.
pub fn describe_release_error(error: &ReleaseError) -> String {
    match error {
        ReleaseError::ArtifactNotFound(id) => {
            format!("artifact missing from the catalog ({})", artifact_id_label(id))
        }
        ReleaseError::IncompleteManifest { missing, release_id } => format!(
            "release {:?} does not cover every non-Provision kind; missing: {}",
            release_id.0,
            missing
                .iter()
                .map(artifact_id_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ReleaseError::ProvisionKindForbidden(id) => format!(
            "Provision-kind artifacts are forbidden inside manifests ({})",
            artifact_id_label(id)
        ),
        ReleaseError::NoBootstrapAuthority => {
            "bootstrap authority has not been seeded yet".to_owned()
        }
        ReleaseError::ArtifactNotVerified(id) => format!(
            "artifact has not reached verified state ({})",
            artifact_id_label(id)
        ),
        ReleaseError::Unauthorized => {
            "caller is not the resolved governance authority; pass --identity with the governance PEM"
                .to_owned()
        }
        ReleaseError::UnknownRelease(release_id) => {
            format!("no release exists under {release_id:?}")
        }
        ReleaseError::ConflictingRelease {
            requested,
            existing,
        } => format!(
            "release id conflict: requested {:?}, already stored under {:?}",
            requested.0, existing.0
        ),
        ReleaseError::NotUniquePerKind {
            kind,
            conflicting,
            release_id,
        } => format!(
            "release {:?} supplies more than one artifact for kind {}: {}",
            release_id.0,
            kind_name(*kind),
            conflicting
                .iter()
                .map(artifact_id_label)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Render a release-install failure.
pub fn describe_install_error(error: &InstallError) -> String {
    match error {
        InstallError::ArtifactNotFound(id) => {
            format!("artifact missing from the catalog ({})", artifact_id_label(id))
        }
        InstallError::NoActiveRelease => "no active release; run `release activate` first".to_owned(),
        InstallError::ManagementCanisterCallFailed(reason) => {
            format!("management-canister step failed: {reason}")
        }
        InstallError::ChunkStoreNotReconciled => {
            "chunk store has not been reconciled yet".to_owned()
        }
        InstallError::TargetCanisterKindForbidden(kind) => format!(
            "target canister kind {} is forbidden for installs",
            kind_name(*kind)
        ),
        InstallError::NoBootstrapAuthority => {
            "bootstrap authority has not been seeded yet".to_owned()
        }
        InstallError::ArtifactNotVerified(id) => format!(
            "artifact has not reached verified state ({})",
            artifact_id_label(id)
        ),
        InstallError::Unauthorized => {
            "caller is not the resolved governance authority; pass --identity with the governance PEM"
                .to_owned()
        }
    }
}

/// Render a binding-install failure.
pub fn describe_admin_install_error(error: &AdminInstallError) -> String {
    match error {
        AdminInstallError::UnknownDeployment(deployment_id) => format!(
            "unknown deployment {deployment_id:?}; only the bootstrap authority may install new bindings"
        ),
        AdminInstallError::AlreadyExists {
            existing_governance,
            deployment_id,
        } => format!(
            "deployment {deployment_id} is already bound (existing governance principal {existing_governance})"
        ),
        AdminInstallError::InvalidState(reason) => format!("binding cannot be installed: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_artifact_api::types::{ArtifactId, CanisterKind, ReleaseId};

    fn id(version: &str) -> ArtifactId {
        ArtifactId::new(CanisterKind::Router, version.to_owned(), [7u8; 32])
    }

    /// Every variant must render non-empty, distinct text mentioning its key data.
    #[test]
    fn ingest_error_renders_both_variants() {
        let server = describe_ingest_error(&IngestError::Server(ArtifactError::Unauthorized));
        assert!(server.contains("governance authority"), "got: {server}");
        let failed = describe_ingest_error(&IngestError::UploadFailed {
            reason: "sha mismatch".into(),
        });
        assert!(failed.contains("sha mismatch"), "got: {failed}");
        assert_ne!(server, failed);
    }

    #[test]
    fn artifact_error_renders_every_variant() {
        let rendered = [
            describe_artifact_error(&ArtifactError::TooManyChunks {
                max: 4096,
                declared: 4097,
            }),
            describe_artifact_error(&ArtifactError::NotProvision(CanisterKind::TextCanister)),
            describe_artifact_error(&ArtifactError::UnknownArtifact(id("1.0.0"))),
            describe_artifact_error(&ArtifactError::Unauthorized),
            describe_artifact_error(&ArtifactError::ChunkHashMismatch {
                chunk_index: 2,
                artifact_id: id("1.0.0"),
            }),
            describe_artifact_error(&ArtifactError::ChunkOutOfRange {
                chunk_index: 9,
                artifact_id: id("1.0.0"),
                declared: 3,
            }),
            describe_artifact_error(&ArtifactError::ConflictingMetadata {
                requested: id("1.0.0"),
                existing: id("1.0.0"),
            }),
            describe_artifact_error(&ArtifactError::FullSha256Mismatch {
                actual: [1u8; 32],
                artifact_id: id("1.0.0"),
                expected: [2u8; 32],
            }),
            describe_artifact_error(&ArtifactError::ArtifactTooLarge {
                max: 512 * 1024 * 1024,
                byte_length: 512 * 1024 * 1024 + 1,
            }),
            describe_artifact_error(&ArtifactError::SemanticVersionTooLong { max: 128 }),
        ];
        assert_eq!(rendered.len(), 10, "one renderer arm per wire variant");
        for text in &rendered {
            assert!(!text.is_empty(), "empty rendering");
        }
        assert!(rendered[0].contains("4097"));
        assert!(rendered[1].contains("TextCanister"));
        assert!(rendered[2].contains("Router 1.0.0"));
        assert!(rendered[5].contains("outside the declared range of 3"));
        // Identical re-publish carries the resume hint; divergent identities do not.
        assert!(rendered[6].contains("safe to continue"));
        assert!(rendered[7].contains("expected:") && rendered[7].contains("actual:"));
    }

    #[test]
    fn release_error_renders_every_variant() {
        assert!(
            describe_release_error(&ReleaseError::ArtifactNotFound(id("1.0.0")))
                .contains("missing")
        );
        assert!(
            describe_release_error(&ReleaseError::IncompleteManifest {
                missing: vec![id("1.0.0")],
                release_id: ReleaseId("r".to_owned()),
            })
            .contains("does not cover every non-Provision kind")
        );
        assert!(
            describe_release_error(&ReleaseError::ProvisionKindForbidden(id("1.0.0")))
                .contains("forbidden inside manifests")
        );
        assert!(describe_release_error(&ReleaseError::NoBootstrapAuthority).contains("seeded"));
        assert!(
            describe_release_error(&ReleaseError::ArtifactNotVerified(id("1.0.0")))
                .contains("verified state")
        );
        assert!(
            describe_release_error(&ReleaseError::Unauthorized).contains("governance authority")
        );
        assert!(
            describe_release_error(&ReleaseError::UnknownRelease(ReleaseId("rel".to_owned())))
                .contains("\"rel\"")
        );
        assert!(
            describe_release_error(&ReleaseError::ConflictingRelease {
                requested: ReleaseId("a".to_owned()),
                existing: ReleaseId("b".to_owned()),
            })
            .contains("already stored under \"b\"")
        );
        assert!(
            describe_release_error(&ReleaseError::NotUniquePerKind {
                kind: CanisterKind::Graph,
                conflicting: vec![id("1.0.0")],
                release_id: ReleaseId("r".to_owned()),
            })
            .contains("kind Graph")
        );
    }

    #[test]
    fn install_and_admin_errors_render_every_variant() {
        assert!(
            describe_install_error(&InstallError::ArtifactNotFound(id("1.0.0")))
                .contains("missing")
        );
        assert!(describe_install_error(&InstallError::NoActiveRelease).contains("activate"));
        assert!(
            describe_install_error(&InstallError::ManagementCanisterCallFailed("boom".into()))
                .contains("boom")
        );
        assert!(
            describe_install_error(&InstallError::ChunkStoreNotReconciled).contains("reconciled")
        );
        assert!(
            describe_install_error(&InstallError::TargetCanisterKindForbidden(
                CanisterKind::Graph
            ))
            .contains("Graph is forbidden")
        );
        assert!(describe_install_error(&InstallError::NoBootstrapAuthority).contains("seeded"));
        assert!(
            describe_install_error(&InstallError::ArtifactNotVerified(id("1.0.0")))
                .contains("verified state")
        );
        assert!(
            describe_install_error(&InstallError::Unauthorized).contains("governance authority")
        );

        assert!(
            describe_admin_install_error(&AdminInstallError::UnknownDeployment("d".into()))
                .contains("\"d\"")
        );
        assert!(
            describe_admin_install_error(&AdminInstallError::AlreadyExists {
                existing_governance: candid::Principal::anonymous(),
                deployment_id: "d".into(),
            })
            .contains("already bound")
        );
        assert!(
            describe_admin_install_error(&AdminInstallError::InvalidState("no seed".into()))
                .contains("no seed")
        );
    }
}
