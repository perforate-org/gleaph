module routerGraphPropertyProjectionTests {
  import routerGraphPropertyProjection.* from "./router_graph_property_projection"

  run healthyDelivery = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(indexApplyPrefix(2))
      .then(graphAcknowledge(2))
      .expect(and {
        canonical == Set("m0"),
        projection == Set(posting("m0", 0), posting("m0", 1)),
        durableQueue == List(),
        protocolSafety,
        graphAcknowledgementWitness,
      })
  }

  run rejectionObservedThenRetry = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(indexReject)
      .expect(and {
        response == Rejected(postingsFor("m0")),
        durableQueue == postingsFor("m0"),
        projection == Set(),
        indexRejectWitness,
      })
      .then(graphObserveRejected)
      .then(deliver)
      .then(indexApplyPrefix(2))
      .then(graphAcknowledge(2))
      .expect(and {
        response == ResponseIdle,
        projection == Set(posting("m0", 0), posting("m0", 1)),
        durableQueue == List(),
        graphAcknowledgementWitness,
      })
  }

  run sameMutationPartialPrefixDrain = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(indexApplyPrefix(1))
      .expect(and {
        projection == Set(posting("m0", 0)),
        response == Applied(List(posting("m0", 0))),
        durableQueue == postingsFor("m0"),
        oldestDurableMutationRank == 0,
      })
      .then(graphAcknowledge(1))
      .expect(and {
        durableQueue == List(posting("m0", 1)),
        oldestDurableMutationRank == 0,
        graphAcknowledgementWitness,
      })
      .then(deliver)
      .then(indexApplyPrefix(1))
      .then(graphAcknowledge(1))
      .expect(and {
        durableQueue == List(),
        projection == Set(posting("m0", 0), posting("m0", 1)),
        protocolSafety,
      })
  }

  run appliedResponseLossDuplicateReplay = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(indexApplyPrefix(2))
      .then(loseAppliedResponse)
      .expect(and {
        response == ResponseIdle,
        projection == Set(posting("m0", 0), posting("m0", 1)),
        durableQueue == postingsFor("m0"),
        appliedResponseLossWitness,
      })
      .then(deliver)
      .then(indexApplyPrefix(2))
      .then(graphAcknowledge(2))
      .expect(and {
        projection == Set(posting("m0", 0), posting("m0", 1)),
        durableQueue == List(),
        protocolSafety,
      })
  }

  run restartDuringRequestThenRetry = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(restart)
      .expect(and {
        durableQueue == postingsFor("m0"),
        delivery == DeliveryIdle,
        response == ResponseIdle,
        restartPending,
        restartWitness,
      })
      .then(recover)
      .then(indexApplyPrefix(2))
      .then(graphAcknowledge(2))
      .expect(and {
        durableQueue == List(),
        projection == Set(posting("m0", 0), posting("m0", 1)),
        protocolSafety,
      })
  }

  run restartAfterIndexApplyBeforeGraphObserveThenReplay = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver)
      .then(indexApplyPrefix(2))
      .then(restart)
      .then(recover)
      .then(indexApplyPrefix(2))
      .then(graphAcknowledge(2))
      .expect(and {
        projection == Set(posting("m0", 0), posting("m0", 1)),
        durableQueue == List(),
        restartPending == false,
        protocolSafety,
      })
  }

  run routerCompleteWithPendingProjection = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(routerComplete("m0"))
      .expect(and {
        routerCompleted == Set("m0"),
        canonical == Set("m0"),
        projection == Set(),
        durableQueue == postingsFor("m0"),
        routerCompletionIndependence,
        protocolSafety,
      })
  }

  run applyWithoutDeliveryFails = init
    .then(routerDispatch("m0"))
    .then(graphCommit("m0"))
    .then(indexApplyPrefix(1).fail())

  run rejectWithoutDeliveryFails = init
    .then(routerDispatch("m0"))
    .then(graphCommit("m0"))
    .then(indexReject.fail())

  run acknowledgeWithoutAppliedResponseFails = init
    .then(routerDispatch("m0"))
    .then(graphCommit("m0"))
    .then(graphAcknowledge(1).fail())

  run observeRejectionWithoutRejectedResponseFails = init
    .then(routerDispatch("m0"))
    .then(graphCommit("m0"))
    .then(graphObserveRejected.fail())

  run overAcknowledgeFails = init
    .then(routerDispatch("m0"))
    .then(graphCommit("m0"))
    .then(deliver)
    .then(indexApplyPrefix(1))
    .then(graphAcknowledge(2).fail())
}
