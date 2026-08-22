module routerVectorTombstoneGcTests {
  import routerVectorTombstoneGc.* from "./router_vector_tombstone_gc"

  run frozenPolicyRetainsFenceAcrossLostResponseReplay = {
    initWithPolicy(Frozen)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(loseVectorResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M12_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .expect(and {
        vectorState.targetClock == Deleted(11),
        vectorState.primaryGraphWatermark == 11,
        vectorState.primaryRouterWatermark == 0,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        protocolSafety,
      })
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(loseVectorResponse)
      .expect(and {
        vectorState.targetClock == Deleted(11),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        noUnsafeFenceCollection,
        noStaleResurrection,
        protocolSafety,
      })
  }

  run maxAcknowledgedCollectsFenceAndResurrectsStaleUpsert = {
    initWithPolicy(MaxAcknowledged)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(loseVectorResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M12_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryGraphWatermark == 11,
        vectorState.primaryRouterWatermark == 12,
        gcCutoff(vectorState, PrimaryLane) == 11,
        vectorState.targetClock == Deleted(11),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
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
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
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
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(loseVectorResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M12_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .expect(and {
        vectorState.primaryGraphWatermark == 11,
        vectorState.primaryRouterWatermark == 0,
        gcCutoff(vectorState, PrimaryLane) == 0,
        vectorState.targetClock == Deleted(11),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        frontierTransport == FrontierIdle,
        protocolSafety,
      })
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 12,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        vectorState.targetClock == Deleted(11),
        protocolSafety,
      })
      .then(observeFrontierResponse)
      .expect(and {
        not(hasIntent(routerState, M10_UPSERT)),
        not(hasIntent(routerState, M12_UPSERT)),
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

  run frontierRetirementIsLaneScoped = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .then(allocateUnrelatedM13)
      .then(admitRouterOperation(M14_SECONDARY_UPSERT))
      .then(dispatchGraph(M14_SECONDARY_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.primaryRouterWatermark == 14,
        vectorState.secondaryRouterWatermark == 0,
        not(hasIntent(routerState, M10_UPSERT)),
        not(hasIntent(routerState, M12_UPSERT)),
        hasIntentInPhase(routerState, M14_SECONDARY_UPSERT, AwaitingFrontier),
        protocolSafety,
      })
  }

  run restartBeforeGraphResponsePreservesExactIntent = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(restart)
      .expect(and {
        graphTransport == GraphIdle,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        routerState.allocatedThrough == 10,
        restartWitness,
        protocolSafety,
      })
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .expect(and {
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        not(hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph)),
        protocolSafety,
      })
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .expect(and {
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(restart)
      .expect(and {
        frontierTransport == FrontierIdle,
        vectorState.primaryRouterWatermark == 10,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        restartWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        not(hasIntent(routerState, M10_UPSERT)),
        protocolSafety,
      })
  }

  run lostGraphResponseRetainsGraphPendingIntent = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(loseGraphResponse)
      .expect(and {
        graphTransport == GraphIdle,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        graphResponseLossWitness,
        protocolSafety,
      })
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .expect(and {
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingVector),
        protocolSafety,
      })
  }

  run graphRejectionResolvesIntentAndReleasesFrontier = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .expect(and {
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        routerState.allocatedThrough == 10,
        contiguousFrontier(routerState, PrimaryLane) == 10,
        graphRejectionResolvedWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 10,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        vectorState.targetClock == Missing,
        protocolSafety,
      })
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .then(observeFrontierResponse)
      .expect(and {
        not(hasIntent(routerState, M10_UPSERT)),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        vectorState.primaryRouterWatermark == 10,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        not(hasIntent(routerState, M10_UPSERT)),
        not(hasIntent(routerState, M12_UPSERT)),
        vectorState.primaryRouterWatermark == 12,
        protocolSafety,
      })
  }

  run partialVectorPrefixRetainsExactSuffix = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M10_UPSERT, M12_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .expect(and {
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingVector),
        routerState.primaryMaxAcknowledged == 10,
        contiguousFrontier(routerState, PrimaryLane) == 11,
        vectorState.targetClock == Deleted(11),
        vectorState.otherClock == Missing,
        partialPrefixRetainedWitness,
        protocolSafety,
      })
  }

  run unrelatedGapAndOtherLaneDoNotCrossBlock = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(allocateUnrelatedM13)
      .then(admitRouterOperation(M14_SECONDARY_UPSERT))
      .then(dispatchGraph(M14_SECONDARY_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M14_SECONDARY_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .expect(and {
        routerState.allocatedThrough == 14,
        contiguousFrontier(routerState, PrimaryLane) == 9,
        contiguousFrontier(routerState, SecondaryLane) == 14,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingGraph),
        hasIntentInPhase(routerState, M14_SECONDARY_UPSERT, AwaitingFrontier),
        protocolSafety,
      })
      .then(dispatchRouterFrontier(SecondaryLane))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 0,
        vectorState.secondaryRouterWatermark == 14,
        hasIntentInPhase(routerState, M14_SECONDARY_UPSERT, AwaitingFrontier),
        laneIsolationWitness,
        protocolSafety,
      })
  }

  run lostFrontierResponsePreservesAppliedWatermark = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphReject)
      .then(observeGraphReject)
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(loseFrontierResponse)
      .expect(and {
        frontierTransport == FrontierIdle,
        vectorState.primaryRouterWatermark == 10,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        frontierResponseLossWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.primaryRouterWatermark == 10,
        not(hasIntent(routerState, M10_UPSERT)),
        frontierResponseObservationWitness,
        protocolSafety,
      })
  }

  run staleLowerFrontierRetryPreservesWatermarkAndSafety = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(dispatchGraph(M10_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M10_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(dispatchGraph(M12_UPSERT))
      .then(graphAccept)
      .then(observeGraphAccept)
      .then(dispatchVectorBatch(List(M12_UPSERT)))
      .then(applyVectorPrefix(1))
      .then(observeVectorResponse)
      .then(dispatchRouterFrontier(PrimaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.primaryRouterWatermark == 12,
        vectorState.targetClock == Deleted(11),
        not(hasIntent(routerState, M10_UPSERT)),
        not(hasIntent(routerState, M12_UPSERT)),
        protocolSafety,
      })
      .then(retryStaleLowerPrimaryFrontier)
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 12,
        vectorState.targetClock == Deleted(11),
        noUnsafeFenceCollection,
        noStaleResurrection,
        protocolSafety,
      })
      .then(gcTargetClock)
      .expect(and {
        vectorState.primaryRouterWatermark == 12,
        vectorState.targetClock == Missing,
        noUnsafeFenceCollection,
        noStaleResurrection,
        protocolSafety,
      })
  }
}
