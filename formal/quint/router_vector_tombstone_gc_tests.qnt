module routerVectorTombstoneGcTests {
  import routerVectorTombstoneGc.* from "./router_vector_tombstone_gc"

  run frozenPolicyRetainsFenceAcrossLostResponseReplay = {
    initWithPolicy(Frozen)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(loseAppliedResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchRouterOperation(M12_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .expect(and {
        vectorState.targetClock == Deleted(11),
        vectorState.graphWatermark == 11,
        vectorState.routerWatermark == 0,
        routerState.outstanding.contains(M10_UPSERT),
        protocolSafety,
      })
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(loseAppliedResponse)
      .expect(and {
        vectorState.targetClock == Deleted(11),
        routerState.outstanding.contains(M10_UPSERT),
        noUnsafeFenceCollection,
        noStaleResurrection,
        protocolSafety,
      })
  }

  run maxAcknowledgedCollectsFenceAndResurrectsStaleUpsert = {
    initWithPolicy(MaxAcknowledged)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(loseAppliedResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchRouterOperation(M12_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .then(publishRouterFrontier)
      .expect(and {
        vectorState.graphWatermark == 11,
        vectorState.routerWatermark == 12,
        gcCutoff(vectorState) == 11,
        vectorState.targetClock == Deleted(11),
        routerState.outstanding.contains(M10_UPSERT),
        not(frontierDoesNotPassOutstanding),
        not(protocolSafety),
      })
      .then(gcTargetClock)
      .expect(and {
        vectorState.targetClock == Missing,
        unsafeFenceCollectionWitness,
        not(noUnsafeFenceCollection),
        not(protocolSafety),
      })
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .expect(and {
        vectorState.targetClock == Live(10),
        staleResurrectionWitness,
        not(noStaleResurrection),
        not(protocolSafety),
      })
  }

  run contiguousFrontierBlocksGcUntilLostResponseIsResolved = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(loseAppliedResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchRouterOperation(M12_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .then(publishRouterFrontier)
      .expect(and {
        vectorState.graphWatermark == 11,
        vectorState.routerWatermark == 9,
        gcCutoff(vectorState) == 9,
        vectorState.targetClock == Deleted(11),
        routerState.outstanding.contains(M10_UPSERT),
        protocolSafety,
      })
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .then(publishRouterFrontier)
      .expect(and {
        vectorState.routerWatermark == 12,
        not(routerState.outstanding.contains(M10_UPSERT)),
        vectorState.targetClock == Deleted(11),
        protocolSafety,
      })
      .then(gcTargetClock)
      .expect(and {
        vectorState.targetClock == Missing,
        noUnsafeFenceCollection,
        noStaleResurrection,
        protocolSafety,
      })
  }

  run restartPreservesDurableIntentAndVectorClock = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(restart)
      .expect(and {
        transport == TransportIdle,
        routerState.issued.contains(M10_UPSERT),
        routerState.outstanding.contains(M10_UPSERT),
        vectorState.targetClock == Live(10),
        restartWitness,
        protocolSafety,
      })
      .then(applyGraphRemoveM11)
      .then(dispatchRouterOperation(M10_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .expect(and {
        vectorState.targetClock == Deleted(11),
        routerState.settled.contains(M10_UPSERT),
        protocolSafety,
      })
  }

  run frontierFunctionsDistinguishHoleFromHigherAcknowledgement = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchRouterOperation(M12_UPSERT))
      .then(vectorApply)
      .then(observeAppliedResponse)
      .expect(and {
        maxAcknowledgedFrontier(routerState) == 12,
        contiguousFrontier(routerState) == 9,
        containsStamp(routerState.outstanding, 10),
        containsStamp(routerState.settled, 12),
      })
  }
}
