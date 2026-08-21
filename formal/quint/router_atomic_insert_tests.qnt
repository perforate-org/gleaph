module routerAtomicInsertTests {
  import routerAtomicInsert.* from "./router_atomic_insert"

  pure val EDGE_TARGET: Target = {
    family: Edge,
    publicFingerprint: F0,
    graphFingerprint: F0,
    mutationId: 0,
    targetShard: 0,
    requestToken: Request0,
  }

  pure val VERTEX_TARGET: Target = {
    family: Vertex,
    publicFingerprint: F0,
    graphFingerprint: F0,
    mutationId: 0,
    targetShard: 0,
    requestToken: Request0,
  }

  pure val MIXED_TARGET: Target = {
    family: Mixed,
    publicFingerprint: F0,
    graphFingerprint: F0,
    mutationId: 0,
    targetShard: 0,
    requestToken: Request0,
  }

  pure val EDGE_RECEIPT: Receipt = {
    family: Edge,
    publicFingerprint: F0,
    graphFingerprint: F0,
    logicalCount: 1,
    allocationCount: 0,
  }

  pure val VERTEX_RECEIPT: Receipt = {
    family: Vertex,
    publicFingerprint: F0,
    graphFingerprint: F0,
    logicalCount: 1,
    allocationCount: 1,
  }

  pure val MIXED_RECEIPT: Receipt = {
    family: Mixed,
    publicFingerprint: F0,
    graphFingerprint: F0,
    logicalCount: 2,
    allocationCount: 1,
  }

  run lostResponseExplicitRetryAdoptsExactReceipt = {
    init
      .then(admitPending(Edge, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(loseCanonicalResponse)
      .expect(and {
        routerRecord == Pending(EDGE_TARGET),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        transport == Idle,
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        canonicalResponseLossWitness,
      })
      .then(queryJournal(ExplicitSameKeyRetry))
      .then(journalRespond(ExplicitSameKeyRetry, ExactActiveCompleted))
      .then(adoptExactReceipt(ExplicitSameKeyRetry))
      .expect(and {
        routerRecord == Committed({ target: EDGE_TARGET, receipt: EDGE_RECEIPT }),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        exactReceiptAdoptionWitness,
        protocolSafety,
      })
  }

  run lostResponseBackgroundRecoveryAdoptsExactReceipt = {
    init
      .then(admitPending(Vertex, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(loseCanonicalResponse)
      .then(queryJournal(BackgroundRecovery))
      .then(journalRespond(BackgroundRecovery, ExactActiveCompleted))
      .then(adoptExactReceipt(BackgroundRecovery))
      .expect(and {
        routerRecord == Committed({
          target: VERTEX_TARGET,
          receipt: VERTEX_RECEIPT,
        }),
        admissionTargetSnapshot == VERTEX_TARGET,
        graphJournal == ActiveCompleted(VERTEX_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        exactReceiptAdoptionWitness,
        protocolSafety,
      })
  }

  run explicitAbsentRedispatchesStoredRequest = {
    init
      .then(admitPending(Mixed, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(restart)
      .then(queryJournal(ExplicitSameKeyRetry))
      .then(journalRespond(ExplicitSameKeyRetry, Absent))
      .then(explicitAbsentRedispatch)
      .expect(and {
        routerRecord == Pending(MIXED_TARGET),
        admissionTargetSnapshot == MIXED_TARGET,
        graphJournal == NoGraphJournal,
        transport == CanonicalRequest,
        canonicalEffectCount == 0,
        graphDispatchCount == 2,
        explicitAbsentRedispatchWitness,
        protocolSafety,
      })
      .then(graphCommit)
      .then(persistCanonicalResponse)
      .expect(and {
        routerRecord == Committed({
          target: MIXED_TARGET,
          receipt: MIXED_RECEIPT,
        }),
        admissionTargetSnapshot == MIXED_TARGET,
        graphJournal == ActiveCompleted(MIXED_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 2,
        protocolSafety,
      })
  }

  run backgroundAbsentNeverRedispatches = {
    init
      .then(admitPending(Edge, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(restart)
      .then(queryJournal(BackgroundRecovery))
      .then(journalRespond(BackgroundRecovery, Absent))
      .expect(and {
        routerRecord == Pending(EDGE_TARGET),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == NoGraphJournal,
        canonicalEffectCount == 0,
        graphDispatchCount == 1,
      })
      .then(observeBackgroundAbsent)
      .expect(and {
        routerRecord == Pending(EDGE_TARGET),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == NoGraphJournal,
        transport == Idle,
        canonicalEffectCount == 0,
        graphDispatchCount == 1,
        backgroundAbsentWitness,
        protocolSafety,
      })
  }

  run nonExactEvidenceStaysPending = {
    init
      .then(admitPending(Vertex, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(loseCanonicalResponse)
      .then(queryJournal(ExplicitSameKeyRetry))
      .then(journalRespond(ExplicitSameKeyRetry, WrongFingerprint))
      .expect(and {
        routerRecord == Pending(VERTEX_TARGET),
        admissionTargetSnapshot == VERTEX_TARGET,
        graphJournal == ActiveCompleted(VERTEX_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
      })
      .then(rejectNonExactEvidence)
      .expect(and {
        routerRecord == Pending(VERTEX_TARGET),
        admissionTargetSnapshot == VERTEX_TARGET,
        graphJournal == ActiveCompleted(VERTEX_RECEIPT),
        transport == Idle,
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        nonExactEvidenceWitness,
        protocolSafety,
      })
  }

  run queryErrorStaysPending = {
    init
      .then(admitPending(Edge, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(loseCanonicalResponse)
      .then(queryJournal(BackgroundRecovery))
      .then(journalRespond(BackgroundRecovery, QueryError))
      .expect(and {
        routerRecord == Pending(EDGE_TARGET),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
      })
      .then(observeQueryError)
      .expect(and {
        routerRecord == Pending(EDGE_TARGET),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        transport == Idle,
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        protocolSafety,
      })
  }

  run restartPreservesPendingAndJournal = {
    init
      .then(admitPending(Mixed, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .expect(and {
        routerRecord == Pending(MIXED_TARGET),
        admissionTargetSnapshot == MIXED_TARGET,
        graphJournal == ActiveCompleted(MIXED_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
      })
      .then(restart)
      .expect(and {
        routerRecord == Pending(MIXED_TARGET),
        admissionTargetSnapshot == MIXED_TARGET,
        transport == Idle,
        graphJournal == ActiveCompleted(MIXED_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        restartWitness,
        protocolSafety,
      })
      .then(queryJournal(BackgroundRecovery))
      .then(journalRespond(BackgroundRecovery, ExactActiveCompleted))
      .then(adoptExactReceipt(BackgroundRecovery))
      .expect(and {
        routerRecord == Committed({
          target: MIXED_TARGET,
          receipt: MIXED_RECEIPT,
        }),
        admissionTargetSnapshot == MIXED_TARGET,
        graphJournal == ActiveCompleted(MIXED_RECEIPT),
        canonicalEffectCount == 1,
        protocolSafety,
      })
  }

  run duplicateGraphDispatchAppliesOnce = {
    init
      .then(admitPending(Vertex, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(loseCanonicalResponse)
      .then(duplicateGraphDispatch)
      .then(graphCommit)
      .then(persistCanonicalResponse)
      .expect(and {
        routerRecord == Committed({
          target: VERTEX_TARGET,
          receipt: VERTEX_RECEIPT,
        }),
        admissionTargetSnapshot == VERTEX_TARGET,
        graphJournal == ActiveCompleted(VERTEX_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 2,
        protocolSafety,
      })
  }

  run exactCommittedReplayReturnsStoredReceipt = {
    init
      .then(admitPending(Edge, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(persistCanonicalResponse)
      .expect(and {
        routerRecord == Committed({ target: EDGE_TARGET, receipt: EDGE_RECEIPT }),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
      })
      .then(exactReplay)
      .expect(and {
        routerRecord == Committed({ target: EDGE_TARGET, receipt: EDGE_RECEIPT }),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        exactReplayWitness,
        protocolSafety,
      })
  }

  run conflictingFingerprintOrFamilyFailsClosed = {
    init
      .then(admitPending(Edge, F0, F0, 0, 0, Request0))
      .then(dispatchCanonical)
      .then(graphCommit)
      .then(persistCanonicalResponse)
      .expect(and {
        routerRecord == Committed({ target: EDGE_TARGET, receipt: EDGE_RECEIPT }),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
      })
      .then(rejectConflict(Vertex, F1))
      .expect(and {
        routerRecord == Committed({ target: EDGE_TARGET, receipt: EDGE_RECEIPT }),
        admissionTargetSnapshot == EDGE_TARGET,
        graphJournal == ActiveCompleted(EDGE_RECEIPT),
        canonicalEffectCount == 1,
        graphDispatchCount == 1,
        conflictWitness,
        protocolSafety,
      })
  }
}
