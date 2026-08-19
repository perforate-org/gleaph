module routerGraphPropertyProjectionTests {
  import routerGraphPropertyProjection.* from "./router_graph_property_projection"

  run healthyDelivery = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver("m0"))
      .then(indexApply("m0"))
      .then(graphFinalize("m0"))
      .expect(and {
        canonical == Set("m0"),
        projection == Set("m0"),
        durableIntent == Set(),
        acknowledged == Set("m0"),
      })
  }

  run rejectionThenRetry = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver("m0"))
      .then(indexReject("m0"))
      .expect(and {
        rejected == Set("m0"),
        durableIntent == Set("m0"),
        projection == Set(),
      })
      .then(recover)
      .then(indexApply("m0"))
      .then(graphFinalize("m0"))
      .expect(and {
        rejected == Set(),
        projection == Set("m0"),
        durableIntent == Set(),
      })
  }

  run duplicateDeliveryIsIdempotent = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver("m0"))
      .then(indexApply("m0"))
      .then(duplicateDelivery("m0"))
      .expect(and {
        projection == Set("m0"),
        acknowledged == Set("m0"),
      })
  }

  run partialAcknowledgementPinsSuffix = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(routerDispatch("m1"))
      .then(graphCommit("m1"))
      .then(deliver("m1"))
      .then(indexApply("m1"))
      .expect(and {
        durableIntent == Set("m0", "m1"),
        indexPendingMinMutationId == 0,
        projection == Set("m1"),
      })
      .then(deliver("m0"))
      .then(indexApply("m0"))
      .then(graphFinalize("m0"))
      .then(graphFinalize("m1"))
      .expect(durableIntent == Set())
  }

  run restartPreservesDurableWork = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(deliver("m0"))
      .then(restart)
      .expect(and {
        durableIntent == Set("m0"),
        volatileInFlight == Set(),
      })
      .then(recover)
      .then(indexApply("m0"))
      .then(graphFinalize("m0"))
      .expect(durableIntent == Set())
  }

  run routerMayCompleteBeforeProjection = {
    init
      .then(routerDispatch("m0"))
      .then(graphCommit("m0"))
      .then(routerComplete("m0"))
      .expect(and {
        routerCompleted == Set("m0"),
        canonical == Set("m0"),
        projection == Set(),
        durableIntent == Set("m0"),
      })
  }
}
