//! Idempotent ingestion driver over an [`ArtifactTransport`].
//!
//! The driver executes the ADR 0087 ordering invariant — declare hashes before bytes flow —
//! and resumes from whatever state the server reports. It is a step-unit API: every transport
//! call is one discrete awaited step, and the function never sleeps or polls. When further
//! progress only requires waiting for server-side verification, it returns
//! [`IngestOutcome::AwaitingVerification`] and hands polling control to the caller.

use crate::pipeline::ArtifactPlan;
use crate::transport::ArtifactTransport;
use crate::types::{ArtifactError, ArtifactId, ArtifactUploadChunkArgs, ArtifactUploadState};

/// Terminal classification of one [`ingest_artifact`] run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The artifact reached durable verified state during this run (or had already reached
    /// it). `verified_at_ns` is `None` when verification was only observable through the
    /// server's conflict signal, whose response carries no timestamp.
    Verified {
        /// Verification completion timestamp when known.
        verified_at_ns: Option<u64>,
    },
    /// Metadata and every chunk were accepted; classification now depends on server-side
    /// streaming verification. Polling control belongs to the caller: poll
    /// `get_status` until it returns `Ok(None)` (verified uploads reclaim their row) or
    /// `Ok(Some(state))` with a terminal state. Reaching this outcome after
    /// [`ingest_artifact`] confirmed metadata means `None` can no longer mean "unpublished".
    AwaitingVerification {
        /// Identity being ingested.
        artifact_id: ArtifactId,
    },
}

/// Failure modes of [`ingest_artifact`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IngestError {
    /// A catalog ingress call was rejected by the server.
    Server(
        /// The server's typed rejection.
        ArtifactError,
    ),
    /// The observed upload state is terminally `Failed` (e.g. full SHA-256 mismatch on an
    /// earlier run); the reason text comes from the server.
    UploadFailed {
        /// Server-provided failure reason.
        reason: String,
    },
}

/// Ingest one planned artifact idempotently: confirm metadata presence, upload exactly the
/// chunks the server has not yet accepted (in strict ascending index order), and classify the
/// resulting state. Safe to re-run at any point; every step either converges toward
/// `verified` or surfaces the server's typed error.
///
/// Ordering contract (ADR 0087): no chunk is sent before metadata presence for the plan's
/// exact identity is confirmed. Confirmation is either a successful `publish_metadata`
/// response or `ArtifactError::ConflictingMetadata { existing == requested == plan id }`,
/// which proves an earlier run published this exact hash declaration. The second case also
/// covers the ambiguous `None`-status resume (published-but-unchunked vs verified-and-
/// reclaimed): pending chunks are re-sent optimistically — duplicate chunks are hash-checked
/// no-ops while unverified, and a conflict signal during upload resolves to
/// [`IngestOutcome::Verified`] because identity equality pins identical bytes.
#[allow(clippy::result_large_err)] // ArtifactError is a wire-mirror type; boxing would distort matching.
pub async fn ingest_artifact<T: ArtifactTransport>(
    plan: &ArtifactPlan<'_>,
    transport: &T,
) -> Result<IngestOutcome, IngestError> {
    let artifact_id = plan.artifact_id.clone();

    // Step 1: observe current upload progress.
    let status = transport
        .get_status(artifact_id.clone())
        .await
        .map_err(IngestError::Server)?;

    if let Some(upload) = &status {
        match &upload.state {
            ArtifactUploadState::Failed { reason } => {
                return Err(IngestError::UploadFailed {
                    reason: reason.clone(),
                });
            }
            // Persisted rows are reclaimed at verify, so this arm only fires if a Verified
            // row is still observable; treat it as done without touching any bytes.
            ArtifactUploadState::Verified { verified_at_ns } => {
                return Ok(IngestOutcome::Verified {
                    verified_at_ns: Some(*verified_at_ns),
                });
            }
            ArtifactUploadState::Receiving | ArtifactUploadState::Verifying => {}
        }
    }

    // Step 2: confirm metadata presence before any byte flows, then derive the pending set.
    let pending: Vec<u32> = if let Some(upload) = &status {
        // An upload row exists, so its metadata row necessarily exists too; skip publishing.
        missing_chunk_indices(plan, &upload.received_chunks)
    } else {
        match transport.publish_metadata(plan.publish_args.clone()).await {
            Ok(_) => all_chunk_indices(plan),
            // Equal identities prove this exact hash declaration is already published.
            // Progress is unknown (unchunked or verified-reclaimed), so fall through to the
            // optimistic chunk resend documented above.
            Err(ArtifactError::ConflictingMetadata {
                existing,
                requested,
            }) if existing == requested && requested == artifact_id => all_chunk_indices(plan),
            Err(err) => return Err(IngestError::Server(err)),
        }
    };

    // Step 3: send pending chunks in strict ascending index order.
    for chunk_index in pending {
        let args_bytes = plan.chunks[chunk_index as usize].to_vec();
        match transport
            .upload_chunk(ArtifactUploadChunkArgs {
                chunk_index,
                artifact_id: artifact_id.clone(),
                bytes: args_bytes,
            })
            .await
        {
            Ok(upload) => {
                if let ArtifactUploadState::Verified { verified_at_ns } = upload.state {
                    return Ok(IngestOutcome::Verified {
                        verified_at_ns: Some(verified_at_ns),
                    });
                }
            }
            // Conflict with equal identities mid-upload means the artifact became verified
            // between calls (the final-chunk verification reclaimed its row).
            Err(ArtifactError::ConflictingMetadata {
                existing,
                requested,
            }) if existing == requested && requested == artifact_id => {
                return Ok(IngestOutcome::Verified {
                    verified_at_ns: None,
                });
            }
            Err(err) => return Err(IngestError::Server(err)),
        }
    }

    // Step 4: nothing left to send and no Verified confirmation arrived — the observed state
    // was fully-received/Verifying. Verification happens server-side; the caller owns pacing.
    Ok(IngestOutcome::AwaitingVerification { artifact_id })
}

/// Indices of declared chunks not yet accepted by the server, ascending.
fn missing_chunk_indices(
    plan: &ArtifactPlan<'_>,
    received: &std::collections::BTreeSet<u32>,
) -> Vec<u32> {
    (0..plan.chunk_count())
        .filter(|index| !received.contains(index))
        .collect()
}

/// Every declared chunk index, ascending.
fn all_chunk_indices(plan: &ArtifactPlan<'_>) -> Vec<u32> {
    (0..plan.chunk_count()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ArtifactPublishMetadataArgs, CanisterKind, MAX_CHUNK_BYTES};

    fn tiny_plan(bytes: &[u8]) -> ArtifactPlan<'_> {
        crate::pipeline::plan_artifact(bytes, CanisterKind::Router, "1.0.0").unwrap()
    }

    #[test]
    fn missing_indices_exclude_received_and_stay_ascending() {
        let bytes = vec![0u8; MAX_CHUNK_BYTES * 3];
        let plan = tiny_plan(&bytes);
        let received = [1u32].into_iter().collect();
        assert_eq!(missing_chunk_indices(&plan, &received), vec![0, 2]);
        assert_eq!(all_chunk_indices(&plan), vec![0, 1, 2]);
    }

    #[test]
    fn publish_args_round_trip_through_plan_fields() {
        let plan = tiny_plan(b"abc");
        assert_eq!(
            plan.publish_args,
            ArtifactPublishMetadataArgs {
                sha256: plan.artifact_id.sha256,
                chunk_hashes: vec![plan.artifact_id.sha256],
                semantic_version: "1.0.0".to_owned(),
                byte_length: 3,
                canister_kind: CanisterKind::Router,
            }
        );
    }
}
