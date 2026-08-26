//! Integration tests for [`ingest_artifact`] over an in-memory fake transport.
//!
//! Each test drives the real driver against canned server states (fresh, partial, verifying,
//! failed, verified-reclaimed) and asserts the exact transport-call sequence, so ordering
//! violations (bytes before metadata confirmation, out-of-order or redundant chunk sends)
//! fail loudly.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use gleaph_artifact_api::types::{
    ArtifactError, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload,
    ArtifactUploadChunkArgs, ArtifactUploadState, CanisterKind, MAX_CHUNK_BYTES,
    ReleaseActivateArgs, ReleaseActivateResult, ReleaseError, ReleaseManifest, ReleasePublishArgs,
};
use gleaph_artifact_api::{
    ArtifactTransport, IngestError, IngestOutcome, ingest_artifact, plan_artifact,
};

/// Arbitrary verification timestamp returned by the fake on the final accepted chunk.
const FAKE_VERIFIED_AT_NS: u64 = 1_748_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    GetStatus,
    PublishMetadata,
    UploadChunk(u32),
}

/// Deterministic stand-in for the Provision catalog.
///
/// Response model:
/// - `get_status` replays `status`.
/// - `publish_metadata` fails with `publish_error` when set, else succeeds.
/// - `upload_chunk` fails with `chunk_errors[index]` when set; otherwise, when
///   `server_verified` is set, it answers with the equal-identity conflict the real server
///   emits for uploads against an already-verified artifact; otherwise it records the chunk
///   and answers `Receiving`, or `Verified` once the last declared chunk lands (mirroring the
///   real synchronous final-chunk verification).
struct FakeCore {
    artifact_id: ArtifactId,
    total_chunks: u32,
    status: Option<ArtifactUpload>,
    publish_error: Option<ArtifactError>,
    chunk_errors: HashMap<u32, ArtifactError>,
    server_verified: bool,
    calls: Vec<Call>,
    received: BTreeSet<u32>,
}

struct FakeTransport {
    core: Mutex<FakeCore>,
}

impl FakeTransport {
    fn new(plan: &gleaph_artifact_api::ArtifactPlan<'_>) -> Self {
        Self {
            core: Mutex::new(FakeCore {
                artifact_id: plan.artifact_id.clone(),
                total_chunks: plan.chunk_count(),
                status: None,
                publish_error: None,
                chunk_errors: HashMap::new(),
                server_verified: false,
                calls: Vec::new(),
                received: BTreeSet::new(),
            }),
        }
    }

    fn with_status(self, status: Option<ArtifactUpload>) -> Self {
        {
            let mut core = self.core.lock().unwrap();
            // Mirror the real server: its accepted-chunk set already contains every chunk the
            // canned status reports as received.
            if let Some(upload) = &status {
                core.received = upload.received_chunks.clone();
            }
            core.status = status;
        }
        self
    }

    fn with_publish_error(self, error: ArtifactError) -> Self {
        self.core.lock().unwrap().publish_error = Some(error);
        self
    }

    fn with_chunk_error(self, index: u32, error: ArtifactError) -> Self {
        self.core.lock().unwrap().chunk_errors.insert(index, error);
        self
    }

    fn with_server_verified(self) -> Self {
        self.core.lock().unwrap().server_verified = true;
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.core.lock().unwrap().calls.clone()
    }
}

fn receiving_status(id: &ArtifactId, received: &[u32]) -> ArtifactUpload {
    ArtifactUpload {
        started_at_ns: 1_000,
        artifact_id: id.clone(),
        received_chunks: received.iter().copied().collect(),
        verified_at_ns: None,
        state: ArtifactUploadState::Receiving,
    }
}

fn verifying_status(id: &ArtifactId, total_chunks: u32) -> ArtifactUpload {
    ArtifactUpload {
        started_at_ns: 1_000,
        artifact_id: id.clone(),
        received_chunks: (0..total_chunks).collect(),
        verified_at_ns: None,
        state: ArtifactUploadState::Verifying,
    }
}

fn failed_status(id: &ArtifactId, reason: &str) -> ArtifactUpload {
    ArtifactUpload {
        started_at_ns: 1_000,
        artifact_id: id.clone(),
        received_chunks: BTreeSet::new(),
        verified_at_ns: None,
        state: ArtifactUploadState::Failed {
            reason: reason.to_owned(),
        },
    }
}

fn verified_status(id: &ArtifactId) -> ArtifactUpload {
    ArtifactUpload {
        started_at_ns: 1_000,
        artifact_id: id.clone(),
        received_chunks: BTreeSet::new(),
        verified_at_ns: Some(FAKE_VERIFIED_AT_NS),
        state: ArtifactUploadState::Verified {
            verified_at_ns: FAKE_VERIFIED_AT_NS,
        },
    }
}

fn equal_identity_conflict(id: &ArtifactId) -> ArtifactError {
    ArtifactError::ConflictingMetadata {
        requested: id.clone(),
        existing: id.clone(),
    }
}

impl ArtifactTransport for FakeTransport {
    async fn publish_metadata(
        &self,
        args: ArtifactPublishMetadataArgs,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        let mut core = self.core.lock().unwrap();
        core.calls.push(Call::PublishMetadata);
        if let Some(error) = &core.publish_error {
            return Err(error.clone());
        }
        Ok(ArtifactMetadata {
            verified: false,
            artifact_id: ArtifactId::new(args.canister_kind, args.semantic_version, args.sha256),
            chunk_hashes: args.chunk_hashes,
            byte_length: args.byte_length,
            created_at_ns: 1_000,
            storage_id: 1,
        })
    }

    async fn upload_chunk(
        &self,
        args: ArtifactUploadChunkArgs,
    ) -> Result<ArtifactUpload, ArtifactError> {
        let mut core = self.core.lock().unwrap();
        core.calls.push(Call::UploadChunk(args.chunk_index));
        if let Some(error) = core.chunk_errors.get(&args.chunk_index) {
            return Err(error.clone());
        }
        assert_eq!(
            args.artifact_id, core.artifact_id,
            "driver must upload against the planned identity"
        );
        if core.server_verified {
            // The real server rejects uploads against a verified artifact with this
            // equal-identity conflict (crates/provision/src/canister/mod.rs:826-841).
            return Err(ArtifactError::ConflictingMetadata {
                requested: core.artifact_id.clone(),
                existing: core.artifact_id.clone(),
            });
        }
        core.received.insert(args.chunk_index);
        let complete = core.received.len() as u32 == core.total_chunks;
        Ok(ArtifactUpload {
            started_at_ns: 1_000,
            artifact_id: args.artifact_id.clone(),
            received_chunks: core.received.clone(),
            verified_at_ns: complete.then_some(FAKE_VERIFIED_AT_NS),
            state: if complete {
                ArtifactUploadState::Verified {
                    verified_at_ns: FAKE_VERIFIED_AT_NS,
                }
            } else {
                ArtifactUploadState::Receiving
            },
        })
    }

    async fn get_status(
        &self,
        _artifact_id: ArtifactId,
    ) -> Result<Option<ArtifactUpload>, ArtifactError> {
        let mut core = self.core.lock().unwrap();
        core.calls.push(Call::GetStatus);
        Ok(core.status.clone())
    }

    async fn release_publish(
        &self,
        _args: ReleasePublishArgs,
    ) -> Result<ReleaseManifest, ReleaseError> {
        panic!("release_publish is outside the ingest-driver contract")
    }

    async fn release_activate(
        &self,
        _args: ReleaseActivateArgs,
    ) -> Result<ReleaseActivateResult, ReleaseError> {
        panic!("release_activate is outside the ingest-driver contract")
    }
}

/// Blocks on a future that never yields `Pending`; no async runtime dependency is needed
/// because the fake transport completes every step synchronously.
fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll};
    let mut future = Box::pin(future);
    let mut cx = Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(output) => output,
        Poll::Pending => unreachable!("fake transport responses are immediately ready"),
    }
}

#[test]
fn fresh_ingest_publishes_then_uploads_all_chunks_in_order() {
    let bytes = vec![0u8; MAX_CHUNK_BYTES * 2 + 5];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan);
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::Verified {
            verified_at_ns: Some(FAKE_VERIFIED_AT_NS),
        }
    );
    assert_eq!(
        transport.calls(),
        vec![
            Call::GetStatus,
            Call::PublishMetadata,
            Call::UploadChunk(0),
            Call::UploadChunk(1),
            Call::UploadChunk(2),
        ],
        "metadata must be published before any byte flows, chunks strictly ascending"
    );
}

#[test]
fn resume_from_partial_receiving_skips_received_and_never_republishes() {
    let bytes = vec![0u8; MAX_CHUNK_BYTES + 9];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport =
        FakeTransport::new(&plan).with_status(Some(receiving_status(&plan.artifact_id, &[0])));
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::Verified {
            verified_at_ns: Some(FAKE_VERIFIED_AT_NS),
        }
    );
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus, Call::UploadChunk(1)],
        "only the missing chunk may be sent; no duplicate publish"
    );
}

#[test]
fn resume_from_verifying_returns_awaiting_verification_without_sending() {
    let bytes = vec![0u8; MAX_CHUNK_BYTES * 2];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan).with_status(Some(verifying_status(
        &plan.artifact_id,
        plan.chunk_count(),
    )));
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::AwaitingVerification {
            artifact_id: plan.artifact_id.clone(),
        }
    );
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus],
        "fully-received artifacts must not be re-uploaded; polling belongs to the caller"
    );
}

#[test]
fn resume_from_failed_state_surfaces_upload_failed_without_side_effects() {
    let bytes = vec![0u8; 4];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan).with_status(Some(failed_status(
        &plan.artifact_id,
        "full SHA-256 mismatch",
    )));
    let error = block_on(ingest_artifact(&plan, &transport)).unwrap_err();

    assert_eq!(
        error,
        IngestError::UploadFailed {
            reason: "full SHA-256 mismatch".to_owned(),
        }
    );
    assert_eq!(transport.calls(), vec![Call::GetStatus]);
}

#[test]
fn verified_entry_status_short_circuits_before_any_write() {
    let bytes = vec![0u8; 4];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan).with_status(Some(verified_status(&plan.artifact_id)));
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::Verified {
            verified_at_ns: Some(FAKE_VERIFIED_AT_NS),
        }
    );
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus],
        "verified artifacts must be detected without republishing or re-uploading"
    );
}

#[test]
fn reclaimed_row_with_equal_identity_conflict_resolves_to_verified() {
    // Server reality: after verification the upload row is reclaimed, so get_status returns
    // None and even identical republishes answer ConflictingMetadata. The driver proves
    // metadata presence via that equal-identity conflict, probes one chunk optimistically,
    // and reads the upload-path conflict signal as "already verified".
    let bytes = vec![0u8; MAX_CHUNK_BYTES + 3];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan)
        .with_publish_error(equal_identity_conflict(&plan.artifact_id))
        .with_server_verified();
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::Verified {
            verified_at_ns: None
        }
    );
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus, Call::PublishMetadata, Call::UploadChunk(0)],
        "the first conflict signal during upload must stop further sends"
    );
}

#[test]
fn conflict_signal_during_upload_stops_remaining_chunks_as_verified() {
    // Mid-run race: status showed Receiving, but verification completed before the next send.
    let bytes = vec![0u8; MAX_CHUNK_BYTES + 6];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan)
        .with_status(Some(receiving_status(&plan.artifact_id, &[0])))
        .with_server_verified();
    let outcome = block_on(ingest_artifact(&plan, &transport)).unwrap();

    assert_eq!(
        outcome,
        IngestOutcome::Verified {
            verified_at_ns: None
        }
    );
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus, Call::UploadChunk(1)],
    );
}

#[test]
fn hard_publish_rejection_blocks_all_chunk_traffic() {
    let bytes = vec![0u8; MAX_CHUNK_BYTES];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let transport = FakeTransport::new(&plan).with_publish_error(ArtifactError::Unauthorized);
    let error = block_on(ingest_artifact(&plan, &transport)).unwrap_err();

    assert_eq!(error, IngestError::Server(ArtifactError::Unauthorized));
    assert_eq!(
        transport.calls(),
        vec![Call::GetStatus, Call::PublishMetadata],
        "no chunk may leave before metadata presence is confirmed"
    );
}

#[test]
fn chunk_rejection_stops_remaining_sends_and_propagates_the_server_error() {
    let bytes = vec![0u8; MAX_CHUNK_BYTES + 6];
    let plan = plan_artifact(&bytes, CanisterKind::Router, "1.0.0").unwrap();
    let unexpected = ArtifactError::UnknownArtifact(plan.artifact_id.clone());
    let transport = FakeTransport::new(&plan).with_chunk_error(1, unexpected.clone());
    let error = block_on(ingest_artifact(&plan, &transport)).unwrap_err();

    assert_eq!(error, IngestError::Server(unexpected));
    assert_eq!(
        transport.calls(),
        vec![
            Call::GetStatus,
            Call::PublishMetadata,
            Call::UploadChunk(0),
            Call::UploadChunk(1),
        ],
        "a failed chunk must abort the run instead of continuing to later indices"
    );
}

#[test]
fn empty_plan_is_rejected_before_any_transport_call_would_exist() {
    let error = plan_artifact(b"", CanisterKind::Graph, "1.0.0").unwrap_err();
    assert_eq!(
        error,
        ArtifactError::TooManyChunks {
            max: gleaph_artifact_api::types::MAX_ARTIFACT_CHUNKS,
            declared: 0,
        }
    );
}
