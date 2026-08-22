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

  run markerlessGraphOnlyRemovalAdvancesAndGcWithoutRetiringPrimaryMarkers = {
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
      .then(applyGraphOnlyRemoveM14)
      .expect(and {
        vectorState.secondaryClock == Deleted(14),
        vectorState.secondaryGraphWatermark == 14,
        vectorState.secondaryRouterWatermark == 0,
        frontierMarkerSnapshot(routerState, SecondaryLane, 14) == Set(),
        markerlessGraphOnlyRemoveWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(SecondaryLane))
      .expect(frontierTransport == FrontierDelivered({
        lane: SecondaryLane,
        frontier: 14,
        markers: Set(),
      }))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.primaryRouterWatermark == 0,
        vectorState.secondaryRouterWatermark == 14,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        vectorState.secondaryClock == Deleted(14),
        frontierResponseObservationWitness,
        protocolSafety,
      })
      .then(gcSecondaryClock)
      .expect(and {
        vectorState.secondaryClock == Missing,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingFrontier),
        markerlessGraphOnlyGcWitness,
        protocolSafety,
      })
  }

  run secondaryLaneRemoveCannotBeAdmittedAsRouterIntent = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(allocateUnrelatedM13)
      .expect(and {
        routerState.allocatedThrough == 13,
        not(hasIntent(routerState, M14_SECONDARY_REMOVE)),
        protocolSafety,
      })
      .then(admitRouterOperation(M14_SECONDARY_REMOVE).fail())
  }

  run capturedPrimaryMarkerRetiresWithoutRetiringSecondaryMarker = {
    initWithPolicy(Contiguous)
      // Test-only state: SecondaryLane markers are unreachable through direct
      // Router admission, but are needed to exercise cross-lane retirement.
      .then(all {
        policy' = policy,
        routerState' = {
          allocatedThrough: 14,
          intents: Set(
            intent(M10_UPSERT, AwaitingFrontier),
            intent(M14_SECONDARY_REMOVE, AwaitingFrontier),
          ),
          primaryMaxAcknowledged: 0,
          secondaryMaxAcknowledged: 0,
        },
        vectorState' = vectorState,
        graphTransport' = GraphIdle,
        vectorTransport' = VectorIdle,
        frontierTransport' = FrontierIdle,
        lastAction' = lastAction,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .expect(frontierTransport == FrontierDelivered({
        lane: PrimaryLane,
        frontier: 14,
        markers: Set(intent(M10_UPSERT, AwaitingFrontier)),
      }))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        routerState.intents == Set(
          intent(M14_SECONDARY_REMOVE, AwaitingFrontier),
        ),
        not(hasIntent(routerState, M10_UPSERT)),
        hasIntentInPhase(routerState, M14_SECONDARY_REMOVE, AwaitingFrontier),
        frontierResponseObservationWitness,
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
      .then(applyGraphOnlyRemoveM14)
      .expect(and {
        routerState.allocatedThrough == 14,
        contiguousFrontier(routerState, PrimaryLane) == 9,
        contiguousFrontier(routerState, SecondaryLane) == 14,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingGraph),
        frontierMarkerSnapshot(routerState, SecondaryLane, 14) == Set(),
        vectorState.secondaryClock == Deleted(14),
        protocolSafety,
      })
      .then(dispatchRouterFrontier(SecondaryLane))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 0,
        vectorState.secondaryRouterWatermark == 14,
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingGraph),
        laneIsolationWitness,
        protocolSafety,
      })
  }

  run sameLaneUnresolvedIntentBoundsMarkerlessFrontier = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(applyGraphRemoveM11)
      .expect(and {
        contiguousFrontier(routerState, PrimaryLane) == 9,
        frontierMarkerSnapshot(routerState, PrimaryLane, 9) == Set(),
        vectorState.targetClock == Deleted(11),
        protocolSafety,
      })
      .then(dispatchRouterFrontier(PrimaryLane))
      .expect(frontierTransport == FrontierDelivered({
        lane: PrimaryLane,
        frontier: 9,
        markers: Set(),
      }))
      .then(applyRouterFrontierMessage)
      .expect(and {
        vectorState.primaryRouterWatermark == 9,
        gcCutoff(vectorState, PrimaryLane) == 9,
        vectorState.targetClock == Deleted(11),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        sameLaneOutstandingBoundWitness,
        protocolSafety,
      })
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.primaryRouterWatermark == 9,
        vectorState.targetClock == Deleted(11),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        protocolSafety,
      })
  }

  run markerlessFrontierLossRestartRetryIsMonotonic = {
    initWithPolicy(Contiguous)
      .then(admitRouterOperation(M10_UPSERT))
      .then(applyGraphRemoveM11)
      .then(admitRouterOperation(M12_UPSERT))
      .then(allocateUnrelatedM13)
      .then(applyGraphOnlyRemoveM14)
      .then(dispatchRouterFrontier(SecondaryLane))
      .then(applyRouterFrontierMessage)
      .then(loseFrontierResponse)
      .expect(and {
        frontierTransport == FrontierIdle,
        vectorState.secondaryRouterWatermark == 14,
        vectorState.secondaryClock == Deleted(14),
        frontierResponseLossWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(SecondaryLane))
      .expect(frontierTransport == FrontierDelivered({
        lane: SecondaryLane,
        frontier: 14,
        markers: Set(),
      }))
      .then(applyRouterFrontierMessage)
      .then(restart)
      .expect(and {
        frontierTransport == FrontierIdle,
        vectorState.secondaryRouterWatermark == 14,
        vectorState.secondaryClock == Deleted(14),
        restartWitness,
        protocolSafety,
      })
      .then(dispatchRouterFrontier(SecondaryLane))
      .then(applyRouterFrontierMessage)
      .then(observeFrontierResponse)
      .expect(and {
        vectorState.secondaryRouterWatermark == 14,
        vectorState.secondaryClock == Deleted(14),
        hasIntentInPhase(routerState, M10_UPSERT, AwaitingGraph),
        hasIntentInPhase(routerState, M12_UPSERT, AwaitingGraph),
        frontierResponseObservationWitness,
        protocolSafety,
      })
      .then(gcSecondaryClock)
      .expect(and {
        vectorState.secondaryRouterWatermark == 14,
        vectorState.secondaryClock == Missing,
        markerlessGraphOnlyGcWitness,
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
