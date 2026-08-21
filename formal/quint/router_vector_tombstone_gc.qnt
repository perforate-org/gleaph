// Bounded Router-Graph-Vector model for direct-ingestion replay and subject-clock GC.
//
// The model compares three Router watermark policies over one Vector target,
// one deleted subject, one unrelated subject, and fixed stamps m10/m11/m12.
// It deliberately abstracts embedding bytes, Candid sizing, ANN maintenance,
// timer cadence, stable-memory representation, and cancellation of a durable
// intent after Graph validation rejects it.
module routerVectorTombstoneGc {
  type Policy = Frozen | MaxAcknowledged | Contiguous
  type Subject = TargetSubject | OtherSubject
  type OperationKind = Upsert | Remove

  type Operation = {
    stamp: int,
    subject: Subject,
    kind: OperationKind,
  }

  type SubjectClock =
    | Missing
    | Live(int)
    | Deleted(int)

  type RouterState = {
    issued: Set[Operation],
    outstanding: Set[Operation],
    settled: Set[Operation],
  }

  type VectorState = {
    targetClock: SubjectClock,
    otherClock: SubjectClock,
    graphWatermark: int,
    routerWatermark: int,
    latestTargetRemoveStamp: int,
  }

  type Transport =
    | TransportIdle
    | Delivered(Operation)
    | AppliedResponse(Operation)

  type ActionTag =
    | InitAction
    | RouterAdmissionAction
    | RouterDispatchAction
    | VectorApplyAction
    | ResponseLossAction
    | ResponseObservationAction
    | GraphRemoveAction
    | RouterFrontierPublicationAction
    | SubjectClockGcAction
    | RestartAction
    | DisabledAction

  pure val M10_UPSERT: Operation = {
    stamp: 10,
    subject: TargetSubject,
    kind: Upsert,
  }

  pure val M12_UPSERT: Operation = {
    stamp: 12,
    subject: OtherSubject,
    kind: Upsert,
  }

  pure val M11_REMOVE: Operation = {
    stamp: 11,
    subject: TargetSubject,
    kind: Remove,
  }

  pure val ROUTER_OPERATIONS: Set[Operation] = Set(M10_UPSERT, M12_UPSERT)
  pure val POLICIES: Set[Policy] = Set(Frozen, MaxAcknowledged, Contiguous)

  var policy: Policy
  var routerState: RouterState
  var vectorState: VectorState
  var transport: Transport
  var lastAction: ActionTag

  pure def clockStamp(clock: SubjectClock): int = match clock {
    | Missing => 0
    | Live(stamp) => stamp
    | Deleted(stamp) => stamp
  }

  pure def applyToClock(clock: SubjectClock, operation: Operation): SubjectClock =
    if (operation.stamp <= clockStamp(clock)) {
      clock
    } else if (operation.kind == Upsert) {
      Live(operation.stamp)
    } else {
      Deleted(operation.stamp)
    }

  pure def applyVectorOperation(state: VectorState, operation: Operation): VectorState =
    if (operation.subject == TargetSubject) {
      { ...state, targetClock: applyToClock(state.targetClock, operation) }
    } else {
      { ...state, otherClock: applyToClock(state.otherClock, operation) }
    }

  pure def applyGraphRemove(state: VectorState): VectorState = {
    val afterRemove = applyVectorOperation(state, M11_REMOVE)
    {
      ...afterRemove,
      graphWatermark: 11,
      latestTargetRemoveStamp: 11,
    }
  }

  pure def containsStamp(operations: Set[Operation], stamp: int): bool =
    operations.exists(operation => operation.stamp == stamp)

  pure def maxAcknowledgedFrontier(state: RouterState): int =
    if (containsStamp(state.settled, 12)) { 12 }
    else if (containsStamp(state.settled, 10)) { 10 }
    else { 0 }

  // This bounded candidate assumes every direct-ingestion stamp is durably
  // recorded in `issued` before its first Graph await. `outstanding` therefore
  // represents every unresolved Router operation that may still be replayed.
  pure def contiguousFrontier(state: RouterState): int =
    if (containsStamp(state.outstanding, 10)) { 9 }
    else if (containsStamp(state.outstanding, 12)) { 11 }
    else if (containsStamp(state.issued, 12)) { 12 }
    else if (containsStamp(state.issued, 10)) { 10 }
    else { 0 }

  pure def computedRouterFrontier(selected: Policy, state: RouterState): int =
    if (selected == Frozen) { 0 }
    else if (selected == MaxAcknowledged) { maxAcknowledgedFrontier(state) }
    else { contiguousFrontier(state) }

  pure def gcCutoff(state: VectorState): int =
    if (state.graphWatermark < state.routerWatermark) {
      state.graphWatermark
    } else {
      state.routerWatermark
    }

  pure def hasReplayableOlderTargetUpsert(state: RouterState, stamp: int): bool =
    state.outstanding.exists(operation => and {
      operation.subject == TargetSubject,
      operation.kind == Upsert,
      operation.stamp < stamp,
    })

  action initWithPolicy(selected: Policy): bool = all {
    POLICIES.contains(selected),
    policy' = selected,
    routerState' = {
      issued: Set(),
      outstanding: Set(),
      settled: Set(),
    },
    vectorState' = {
      targetClock: Missing,
      otherClock: Missing,
      graphWatermark: 0,
      routerWatermark: 0,
      latestTargetRemoveStamp: 0,
    },
    transport' = TransportIdle,
    lastAction' = InitAction,
  }

  action init: bool = {
    nondet selected = POLICIES.oneOf()
    initWithPolicy(selected)
  }

  action initFrozen: bool = initWithPolicy(Frozen)
  action initMaxAcknowledged: bool = initWithPolicy(MaxAcknowledged)
  action initContiguous: bool = initWithPolicy(Contiguous)

  action disabled: bool = all {
    false,
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    transport' = transport,
    lastAction' = DisabledAction,
  }

  // Candidate frontier prerequisite: admission records the exact operation as
  // outstanding before any Graph or Vector message boundary.
  action admitRouterOperation(operation: Operation): bool = all {
    ROUTER_OPERATIONS.contains(operation),
    not(routerState.issued.contains(operation)),
    or {
      operation == M10_UPSERT,
      and {
        operation == M12_UPSERT,
        vectorState.latestTargetRemoveStamp == 11,
      },
    },
    policy' = policy,
    routerState' = {
      issued: routerState.issued.union(Set(operation)),
      outstanding: routerState.outstanding.union(Set(operation)),
      settled: routerState.settled,
    },
    vectorState' = vectorState,
    transport' = transport,
    lastAction' = RouterAdmissionAction,
  }

  val routerAdmissionWitness: bool = lastAction == RouterAdmissionAction

  action dispatchRouterOperation(operation: Operation): bool = all {
    routerState.outstanding.contains(operation),
    transport == TransportIdle,
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    transport' = Delivered(operation),
    lastAction' = RouterDispatchAction,
  }

  val routerDispatchWitness: bool = lastAction == RouterDispatchAction

  action vectorApply: bool = match transport {
    | Delivered(operation) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = applyVectorOperation(vectorState, operation),
        transport' = AppliedResponse(operation),
        lastAction' = VectorApplyAction,
      }
    | _ => disabled
  }

  val vectorApplyWitness: bool = lastAction == VectorApplyAction

  action loseAppliedResponse: bool = match transport {
    | AppliedResponse(_) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        transport' = TransportIdle,
        lastAction' = ResponseLossAction,
      }
    | _ => disabled
  }

  action observeAppliedResponse: bool = match transport {
    | AppliedResponse(operation) => all {
        routerState.outstanding.contains(operation),
        policy' = policy,
        routerState' = {
          issued: routerState.issued,
          outstanding: routerState.outstanding.exclude(Set(operation)),
          settled: routerState.settled.union(Set(operation)),
        },
        vectorState' = vectorState,
        transport' = TransportIdle,
        lastAction' = ResponseObservationAction,
      }
    | _ => disabled
  }

  val responseLossWitness: bool = lastAction == ResponseLossAction
  val responseObservationWitness: bool = lastAction == ResponseObservationAction

  // The attached Graph applies its m11 remove and advances only its own
  // watermark. Router response observation remains independent.
  action applyGraphRemoveM11: bool = all {
    vectorState.latestTargetRemoveStamp == 0,
    routerState.issued.contains(M10_UPSERT),
    policy' = policy,
    routerState' = routerState,
    vectorState' = applyGraphRemove(vectorState),
    transport' = transport,
    lastAction' = GraphRemoveAction,
  }

  val graphRemoveWitness: bool = lastAction == GraphRemoveAction

  action publishRouterFrontier: bool = {
    val frontier = computedRouterFrontier(policy, routerState)
    all {
      frontier > vectorState.routerWatermark,
      policy' = policy,
      routerState' = routerState,
      vectorState' = { ...vectorState, routerWatermark: frontier },
      transport' = transport,
      lastAction' = RouterFrontierPublicationAction,
    }
  }

  action gcTargetClock: bool = match vectorState.targetClock {
    | Deleted(stamp) => all {
        stamp <= gcCutoff(vectorState),
        policy' = policy,
        routerState' = routerState,
        vectorState' = { ...vectorState, targetClock: Missing },
        transport' = transport,
        lastAction' = SubjectClockGcAction,
      }
    | _ => disabled
  }

  action restart: bool = all {
    transport != TransportIdle,
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    transport' = TransportIdle,
    lastAction' = RestartAction,
  }

  val routerFrontierPublicationWitness: bool =
    lastAction == RouterFrontierPublicationAction
  val subjectClockGcWitness: bool = lastAction == SubjectClockGcAction
  val restartWitness: bool = lastAction == RestartAction

  val routerOwnershipPartition: bool = and {
    routerState.issued == routerState.outstanding.union(routerState.settled),
    routerState.outstanding.intersect(routerState.settled).size() == 0,
  }

  val frontierDoesNotPassOutstanding: bool =
    routerState.outstanding.forall(operation =>
      operation.stamp > vectorState.routerWatermark
    )

  // Removing a subject clock is unsafe while an older exact Router operation
  // remains durable and replayable.
  val noUnsafeFenceCollection: bool = match vectorState.targetClock {
    | Missing => or {
        vectorState.latestTargetRemoveStamp == 0,
        not(hasReplayableOlderTargetUpsert(
          routerState,
          vectorState.latestTargetRemoveStamp,
        )),
      }
    | _ => true
  }

  // Once m11 remove has applied, no older replay may restore a live clock.
  val noStaleResurrection: bool = match vectorState.targetClock {
    | Live(stamp) => or {
        vectorState.latestTargetRemoveStamp == 0,
        stamp >= vectorState.latestTargetRemoveStamp,
      }
    | _ => true
  }

  val protocolSafety: bool = and {
    routerOwnershipPartition,
    frontierDoesNotPassOutstanding,
    noUnsafeFenceCollection,
    noStaleResurrection,
  }

  val unsafeFenceCollectionWitness: bool = and {
    policy == MaxAcknowledged,
    vectorState.latestTargetRemoveStamp == 11,
    vectorState.targetClock == Missing,
    routerState.outstanding.contains(M10_UPSERT),
  }

  val staleResurrectionWitness: bool = and {
    policy == MaxAcknowledged,
    vectorState.targetClock == Live(10),
    vectorState.latestTargetRemoveStamp == 11,
  }

  action step: bool = {
    nondet operation = ROUTER_OPERATIONS.oneOf()
    any {
      admitRouterOperation(operation),
      dispatchRouterOperation(operation),
      vectorApply,
      loseAppliedResponse,
      observeAppliedResponse,
      applyGraphRemoveM11,
      publishRouterFrontier,
      gcTargetClock,
      restart,
    }
  }
}
