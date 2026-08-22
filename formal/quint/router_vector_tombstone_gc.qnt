// Bounded Router-Graph-Vector model for direct-ingestion replay and subject-clock GC.
//
// The model compares three Router watermark policies over two target/shard lanes.
// The Contiguous candidate derives each lane's frontier from the durable global
// mutation allocation ceiling and exact unresolved Router intents. Admission
// persists an intent before its first Graph await; Graph rejection and exact
// Vector-prefix observation move it to a durable frontier marker. Frontier
// publication, apply, response loss, marker retirement, GC, and restart remain
// separate actions.
// Embedding bytes, Candid sizing, ANN maintenance, timer cadence, stable-memory
// representation, fairness, and topology changes are deliberately abstracted.
module routerVectorTombstoneGc {
  type Policy = Frozen | MaxAcknowledged | Contiguous
  type Lane = PrimaryLane | SecondaryLane
  type Subject = TargetSubject | OtherSubject | SecondarySubject
  type OperationKind = Upsert | Remove
  type IntentPhase = AwaitingGraph | AwaitingVector | AwaitingFrontier

  type Operation = {
    stamp: int,
    lane: Lane,
    subject: Subject,
    kind: OperationKind,
  }

  type Intent = {
    operation: Operation,
    phase: IntentPhase,
  }

  type SubjectClock =
    | Missing
    | Live(int)
    | Deleted(int)

  type RouterState = {
    allocatedThrough: int,
    intents: Set[Intent],
    primaryMaxAcknowledged: int,
    secondaryMaxAcknowledged: int,
  }

  type VectorState = {
    targetClock: SubjectClock,
    otherClock: SubjectClock,
    secondaryClock: SubjectClock,
    primaryGraphWatermark: int,
    primaryRouterWatermark: int,
    secondaryGraphWatermark: int,
    secondaryRouterWatermark: int,
    latestTargetRemoveStamp: int,
  }

  type GraphTransport =
    | GraphIdle
    | GraphDelivered(Operation)
    | GraphAccepted(Operation)
    | GraphRejected(Operation)

  type VectorAppliedBatch = {
    operations: List[Operation],
    applied: int,
  }

  type VectorTransport =
    | VectorIdle
    | VectorDelivered(List[Operation])
    | VectorApplied(VectorAppliedBatch)

  type FrontierMessage = {
    lane: Lane,
    frontier: int,
    markers: Set[Intent],
  }

  type FrontierTransport =
    | FrontierIdle
    | FrontierDelivered(FrontierMessage)
    | FrontierApplied(FrontierMessage)

  type ActionTag =
    | InitAction
    | RouterAdmissionAction
    | UnrelatedAllocationAction
    | GraphDispatchAction
    | GraphAcceptAction
    | GraphRejectAction
    | GraphResponseLossAction
    | GraphAcceptObservationAction
    | GraphRejectObservationAction
    | VectorDispatchAction
    | VectorApplyAction
    | VectorResponseLossAction
    | VectorResponseObservationAction
    | GraphRemoveAction
    | FrontierDispatchAction
    | FrontierApplyAction
    | FrontierResponseLossAction
    | FrontierResponseObservationAction
    | SubjectClockGcAction
    | RestartAction
    | DisabledAction

  pure val M10_UPSERT: Operation = {
    stamp: 10,
    lane: PrimaryLane,
    subject: TargetSubject,
    kind: Upsert,
  }

  pure val M12_UPSERT: Operation = {
    stamp: 12,
    lane: PrimaryLane,
    subject: OtherSubject,
    kind: Upsert,
  }

  pure val M14_SECONDARY_UPSERT: Operation = {
    stamp: 14,
    lane: SecondaryLane,
    subject: SecondarySubject,
    kind: Upsert,
  }

  pure val M11_REMOVE: Operation = {
    stamp: 11,
    lane: PrimaryLane,
    subject: TargetSubject,
    kind: Remove,
  }

  pure val ROUTER_OPERATIONS: Set[Operation] =
    Set(M10_UPSERT, M12_UPSERT, M14_SECONDARY_UPSERT)
  pure val LANES: Set[Lane] = Set(PrimaryLane, SecondaryLane)

  var policy: Policy
  var routerState: RouterState
  var vectorState: VectorState
  var graphTransport: GraphTransport
  var vectorTransport: VectorTransport
  var frontierTransport: FrontierTransport
  var lastAction: ActionTag

  pure def intent(operation: Operation, phase: IntentPhase): Intent = {
    operation: operation,
    phase: phase,
  }

  pure def hasIntentInPhase(
    state: RouterState,
    operation: Operation,
    phase: IntentPhase,
  ): bool = state.intents.contains(intent(operation, phase))

  pure def hasIntent(state: RouterState, operation: Operation): bool =
    state.intents.exists(current => current.operation == operation)

  pure def addGraphIntent(state: RouterState, operation: Operation): RouterState = {
    ...state,
    allocatedThrough: operation.stamp,
    intents: state.intents.union(Set(intent(operation, AwaitingGraph))),
  }

  pure def acceptGraphIntent(state: RouterState, operation: Operation): RouterState = {
    ...state,
    intents: state.intents
      .exclude(Set(intent(operation, AwaitingGraph)))
      .union(Set(intent(operation, AwaitingVector))),
  }

  // Resolution is durable before any frontier publication await. The marker
  // is no longer replayable work, but remains owned until an observed frontier
  // response retires this exact row.
  pure def resolveIntentToFrontier(
    state: RouterState,
    operation: Operation,
    phase: IntentPhase,
  ): RouterState = {
    ...state,
    intents: state.intents
      .exclude(Set(intent(operation, phase)))
      .union(Set(intent(operation, AwaitingFrontier))),
  }

  pure def retireFrontierMarkers(
    state: RouterState,
    markers: Set[Intent],
  ): RouterState = {
    ...state,
    intents: state.intents.exclude(markers),
  }

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
    } else if (operation.subject == OtherSubject) {
      { ...state, otherClock: applyToClock(state.otherClock, operation) }
    } else {
      { ...state, secondaryClock: applyToClock(state.secondaryClock, operation) }
    }

  pure def applyOperations(
    state: VectorState,
    operations: List[Operation],
  ): VectorState = operations.foldl(state, (current, operation) =>
    applyVectorOperation(current, operation)
  )

  pure def appliedPrefix(
    operations: List[Operation],
    applied: int,
  ): List[Operation] =
    if (applied == 0) { List() }
    else if (applied == 1) { List(operations.head()) }
    else { operations }

  pure def maxOperationStamp(operations: List[Operation]): int =
    operations.foldl(0, (current, operation) =>
      if (operation.stamp > current) { operation.stamp } else { current }
    )

  pure def markAcknowledgedIntentsAwaitingFrontier(
    state: RouterState,
    operations: List[Operation],
  ): RouterState = operations.foldl(state, (current, operation) =>
    resolveIntentToFrontier(current, operation, AwaitingVector)
  )

  pure def maxAcknowledgedForLane(state: RouterState, lane: Lane): int =
    if (lane == PrimaryLane) {
      state.primaryMaxAcknowledged
    } else {
      state.secondaryMaxAcknowledged
    }

  pure def observeAcknowledgedOperations(
    state: RouterState,
    operations: List[Operation],
  ): RouterState = {
    val resolved = markAcknowledgedIntentsAwaitingFrontier(state, operations)
    if (operations.length() == 0) {
      resolved
    } else {
      val lane = operations.head().lane
      val observed = maxOperationStamp(operations)
      if (lane == PrimaryLane) {
        {
          ...resolved,
          primaryMaxAcknowledged:
            if (observed > resolved.primaryMaxAcknowledged) {
              observed
            } else {
              resolved.primaryMaxAcknowledged
            },
        }
      } else {
        {
          ...resolved,
          secondaryMaxAcknowledged:
            if (observed > resolved.secondaryMaxAcknowledged) {
              observed
            } else {
              resolved.secondaryMaxAcknowledged
            },
        }
      }
    }
  }

  pure def hasUnresolvedStamp(
    state: RouterState,
    lane: Lane,
    stamp: int,
  ): bool = state.intents.exists(current => and {
    current.phase != AwaitingFrontier,
    current.operation.lane == lane,
    current.operation.stamp == stamp,
  })

  // Publication captures only durable resolution markers in this exact lane
  // and safe prefix. Markers above the captured frontier remain for a later
  // call and are therefore never retired by an older response.
  pure def frontierMarkerSnapshot(
    state: RouterState,
    lane: Lane,
    frontier: int,
  ): Set[Intent] = state.intents.filter(current => and {
    current.phase == AwaitingFrontier,
    current.operation.lane == lane,
    current.operation.stamp <= frontier,
  })

  // Contiguous publication is marker-driven: a lane with no safe marker has
  // no durable work to publish and remains at its current Vector watermark.
  // This keeps the exact-lane marker set as the sole lane-discovery owner.

  // The bounded operation domain has stamps 10, 12, and 14. The oldest
  // unresolved stamp in one exact target/shard lane blocks only that lane.
  pure def contiguousFrontier(state: RouterState, lane: Lane): int =
    if (hasUnresolvedStamp(state, lane, 10)) { 9 }
    else if (hasUnresolvedStamp(state, lane, 12)) { 11 }
    else if (hasUnresolvedStamp(state, lane, 14)) { 13 }
    else { state.allocatedThrough }

  pure def computedRouterFrontier(
    selected: Policy,
    state: RouterState,
    lane: Lane,
  ): int =
    if (selected == Frozen) { 0 }
    else if (selected == MaxAcknowledged) {
      maxAcknowledgedForLane(state, lane)
    } else {
      contiguousFrontier(state, lane)
    }

  pure def graphWatermarkForLane(state: VectorState, lane: Lane): int =
    if (lane == PrimaryLane) {
      state.primaryGraphWatermark
    } else {
      state.secondaryGraphWatermark
    }

  pure def routerWatermarkForLane(state: VectorState, lane: Lane): int =
    if (lane == PrimaryLane) {
      state.primaryRouterWatermark
    } else {
      state.secondaryRouterWatermark
    }

  pure def applyRouterFrontier(
    state: VectorState,
    message: FrontierMessage,
  ): VectorState =
    if (message.lane == PrimaryLane) {
      {
        ...state,
        primaryRouterWatermark:
          if (message.frontier > state.primaryRouterWatermark) {
            message.frontier
          } else {
            state.primaryRouterWatermark
          },
      }
    } else {
      {
        ...state,
        secondaryRouterWatermark:
          if (message.frontier > state.secondaryRouterWatermark) {
            message.frontier
          } else {
            state.secondaryRouterWatermark
          },
      }
    }

  pure def gcCutoff(state: VectorState, lane: Lane): int = {
    val graph = graphWatermarkForLane(state, lane)
    val router = routerWatermarkForLane(state, lane)
    if (graph < router) { graph } else { router }
  }

  pure def applyGraphRemove(state: VectorState): VectorState = {
    val afterRemove = applyVectorOperation(state, M11_REMOVE)
    {
      ...afterRemove,
      primaryGraphWatermark: 11,
      latestTargetRemoveStamp: 11,
    }
  }

  pure def hasReplayableOlderTargetUpsert(state: RouterState, stamp: int): bool =
    state.intents.exists(current => and {
      current.phase != AwaitingFrontier,
      current.operation.lane == PrimaryLane,
      current.operation.subject == TargetSubject,
      current.operation.kind == Upsert,
      current.operation.stamp < stamp,
    })

  pure def sameLane(operations: List[Operation]): bool =
    if (operations.length() == 0) {
      false
    } else {
      operations.indices().forall(index =>
        operations.nth(index).lane == operations.head().lane
      )
    }

  pure def strictlyAscendingBatch(operations: List[Operation]): bool =
    if (operations.length() == 1) {
      true
    } else if (operations.length() == 2) {
      operations.head().stamp < operations.nth(1).stamp
    } else {
      false
    }

  pure def exactVectorBatchOwned(
    state: RouterState,
    operations: List[Operation],
  ): bool = and {
    operations.length() > 0,
    operations.length() <= 2,
    sameLane(operations),
    operations.indices().forall(index =>
      hasIntentInPhase(state, operations.nth(index), AwaitingVector)
    ),
    strictlyAscendingBatch(operations),
  }

  action initWithPolicy(selected: Policy): bool = all {
    policy' = selected,
    routerState' = {
      allocatedThrough: 9,
      intents: Set(),
      primaryMaxAcknowledged: 0,
      secondaryMaxAcknowledged: 0,
    },
    vectorState' = {
      targetClock: Missing,
      otherClock: Missing,
      secondaryClock: Missing,
      primaryGraphWatermark: 0,
      primaryRouterWatermark: 0,
      secondaryGraphWatermark: 0,
      secondaryRouterWatermark: 0,
      latestTargetRemoveStamp: 0,
    },
    graphTransport' = GraphIdle,
    vectorTransport' = VectorIdle,
    frontierTransport' = FrontierIdle,
    lastAction' = InitAction,
  }

  action init: bool = {
    nondet selected = Set(Frozen, MaxAcknowledged, Contiguous).oneOf()
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
    graphTransport' = graphTransport,
    vectorTransport' = vectorTransport,
    frontierTransport' = frontierTransport,
    lastAction' = DisabledAction,
  }

  // Allocation and exact-intent persistence are one synchronous Router action.
  // There is no await between advancing the durable ceiling and owning the op.
  action admitRouterOperation(operation: Operation): bool = all {
    ROUTER_OPERATIONS.contains(operation),
    operation.stamp == routerState.allocatedThrough + 1,
    not(hasIntent(routerState, operation)),
    policy' = policy,
    routerState' = addGraphIntent(routerState, operation),
    vectorState' = vectorState,
    graphTransport' = graphTransport,
    vectorTransport' = vectorTransport,
    frontierTransport' = frontierTransport,
    lastAction' = RouterAdmissionAction,
  }

  // Stamp 13 belongs to unrelated mutation work. It advances the durable global
  // allocation ceiling but creates no direct-vector intent in either lane.
  action allocateUnrelatedM13: bool = all {
    routerState.allocatedThrough == 12,
    policy' = policy,
    routerState' = { ...routerState, allocatedThrough: 13 },
    vectorState' = vectorState,
    graphTransport' = graphTransport,
    vectorTransport' = vectorTransport,
    frontierTransport' = frontierTransport,
    lastAction' = UnrelatedAllocationAction,
  }

  action dispatchGraph(operation: Operation): bool = all {
    hasIntentInPhase(routerState, operation, AwaitingGraph),
    graphTransport == GraphIdle,
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    graphTransport' = GraphDelivered(operation),
    vectorTransport' = vectorTransport,
    frontierTransport' = frontierTransport,
    lastAction' = GraphDispatchAction,
  }

  action graphAccept: bool = match graphTransport {
    | GraphDelivered(operation) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = GraphAccepted(operation),
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphAcceptAction,
      }
    | _ => disabled
  }

  action graphReject: bool = match graphTransport {
    | GraphDelivered(operation) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = GraphRejected(operation),
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphRejectAction,
      }
    | _ => disabled
  }

  action loseGraphResponse: bool = match graphTransport {
    | GraphAccepted(_) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = GraphIdle,
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphResponseLossAction,
      }
    | GraphRejected(_) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = GraphIdle,
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphResponseLossAction,
      }
    | _ => disabled
  }

  action observeGraphAccept: bool = match graphTransport {
    | GraphAccepted(operation) => all {
        hasIntentInPhase(routerState, operation, AwaitingGraph),
        policy' = policy,
        routerState' = acceptGraphIntent(routerState, operation),
        vectorState' = vectorState,
        graphTransport' = GraphIdle,
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphAcceptObservationAction,
      }
    | _ => disabled
  }

  action observeGraphReject: bool = match graphTransport {
    | GraphRejected(operation) => all {
        hasIntentInPhase(routerState, operation, AwaitingGraph),
        policy' = policy,
        routerState' = resolveIntentToFrontier(
          routerState,
          operation,
          AwaitingGraph,
        ),
        vectorState' = vectorState,
        graphTransport' = GraphIdle,
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = GraphRejectObservationAction,
      }
    | _ => disabled
  }

  action dispatchVectorBatch(operations: List[Operation]): bool = all {
    vectorTransport == VectorIdle,
    exactVectorBatchOwned(routerState, operations),
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    graphTransport' = graphTransport,
    vectorTransport' = VectorDelivered(operations),
    frontierTransport' = frontierTransport,
    lastAction' = VectorDispatchAction,
  }

  action applyVectorPrefix(applied: int): bool = match vectorTransport {
    | VectorDelivered(operations) => all {
        applied > 0,
        applied <= operations.length(),
        policy' = policy,
        routerState' = routerState,
        vectorState' = applyOperations(vectorState, appliedPrefix(operations, applied)),
        graphTransport' = graphTransport,
        vectorTransport' = VectorApplied({ operations: operations, applied: applied }),
        frontierTransport' = frontierTransport,
        lastAction' = VectorApplyAction,
      }
    | _ => disabled
  }

  action loseVectorResponse: bool = match vectorTransport {
    | VectorApplied(_) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = graphTransport,
        vectorTransport' = VectorIdle,
        frontierTransport' = frontierTransport,
        lastAction' = VectorResponseLossAction,
      }
    | _ => disabled
  }

  action observeVectorResponse: bool = match vectorTransport {
    | VectorApplied(response) => {
        val acknowledged = appliedPrefix(response.operations, response.applied)
        all {
          policy' = policy,
          routerState' = observeAcknowledgedOperations(routerState, acknowledged),
          vectorState' = vectorState,
          graphTransport' = graphTransport,
          vectorTransport' = VectorIdle,
          frontierTransport' = frontierTransport,
          lastAction' = VectorResponseObservationAction,
        }
      }
    | _ => disabled
  }

  // The Graph remove consumes global stamp 11 and advances only the Graph-owned
  // watermark for the primary lane.
  action applyGraphRemoveM11: bool = all {
    routerState.allocatedThrough == 10,
    vectorState.latestTargetRemoveStamp == 0,
    policy' = policy,
    routerState' = { ...routerState, allocatedThrough: 11 },
    vectorState' = applyGraphRemove(vectorState),
    graphTransport' = graphTransport,
    vectorTransport' = vectorTransport,
    frontierTransport' = frontierTransport,
    lastAction' = GraphRemoveAction,
  }

  action dispatchRouterFrontier(lane: Lane): bool = {
    val frontier = computedRouterFrontier(policy, routerState, lane)
    val markers = frontierMarkerSnapshot(routerState, lane, frontier)
    all {
      frontierTransport == FrontierIdle,
      or {
        frontier > routerWatermarkForLane(vectorState, lane),
        markers.size() > 0,
      },
      or {
        policy != Contiguous,
        markers.size() > 0,
      },
      policy' = policy,
      routerState' = routerState,
      vectorState' = vectorState,
      graphTransport' = graphTransport,
      vectorTransport' = vectorTransport,
      frontierTransport' = FrontierDelivered({
        lane: lane,
        frontier: frontier,
        markers: markers,
      }),
      lastAction' = FrontierDispatchAction,
    }
  }

  // A delayed retry may carry a lower frontier than one already applied.
  // Keep this deterministic-test fault injection separate from Router frontier
  // derivation so the receiver's monotonic watermark contract is exercised
  // directly without broadening the sampled transition relation.
  action retryStaleLowerPrimaryFrontier: bool = all {
    policy == Contiguous,
    frontierTransport == FrontierIdle,
    vectorState.primaryRouterWatermark >= 12,
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    graphTransport' = graphTransport,
    vectorTransport' = vectorTransport,
    frontierTransport' = FrontierDelivered({
      lane: PrimaryLane,
      frontier: 10,
      markers: Set(),
    }),
    lastAction' = FrontierDispatchAction,
  }

  action applyRouterFrontierMessage: bool = match frontierTransport {
    | FrontierDelivered(message) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = applyRouterFrontier(vectorState, message),
        graphTransport' = graphTransport,
        vectorTransport' = vectorTransport,
        frontierTransport' = FrontierApplied(message),
        lastAction' = FrontierApplyAction,
      }
    | _ => disabled
  }

  action loseFrontierResponse: bool = match frontierTransport {
    | FrontierApplied(_) => all {
        policy' = policy,
        routerState' = routerState,
        vectorState' = vectorState,
        graphTransport' = graphTransport,
        vectorTransport' = vectorTransport,
        frontierTransport' = FrontierIdle,
        lastAction' = FrontierResponseLossAction,
      }
    | _ => disabled
  }

  action observeFrontierResponse: bool = match frontierTransport {
    | FrontierApplied(message) => all {
        message.markers.subseteq(routerState.intents),
        policy' = policy,
        routerState' = retireFrontierMarkers(routerState, message.markers),
        vectorState' = vectorState,
        graphTransport' = graphTransport,
        vectorTransport' = vectorTransport,
        frontierTransport' = FrontierIdle,
        lastAction' = FrontierResponseObservationAction,
      }
    | _ => disabled
  }

  action gcTargetClock: bool = match vectorState.targetClock {
    | Deleted(stamp) => all {
        stamp <= gcCutoff(vectorState, PrimaryLane),
        policy' = policy,
        routerState' = routerState,
        vectorState' = { ...vectorState, targetClock: Missing },
        graphTransport' = graphTransport,
        vectorTransport' = vectorTransport,
        frontierTransport' = frontierTransport,
        lastAction' = SubjectClockGcAction,
      }
    | _ => disabled
  }

  action restart: bool = all {
    or {
      graphTransport != GraphIdle,
      vectorTransport != VectorIdle,
      frontierTransport != FrontierIdle,
    },
    policy' = policy,
    routerState' = routerState,
    vectorState' = vectorState,
    graphTransport' = GraphIdle,
    vectorTransport' = VectorIdle,
    frontierTransport' = FrontierIdle,
    lastAction' = RestartAction,
  }

  val routerAdmissionWitness: bool = lastAction == RouterAdmissionAction
  val unrelatedAllocationWitness: bool = lastAction == UnrelatedAllocationAction
  val graphDispatchWitness: bool = lastAction == GraphDispatchAction
  val graphAcceptWitness: bool = lastAction == GraphAcceptAction
  val graphRejectWitness: bool = lastAction == GraphRejectAction
  val graphResponseLossWitness: bool = lastAction == GraphResponseLossAction
  val graphAcceptObservationWitness: bool = lastAction == GraphAcceptObservationAction
  val graphRejectObservationWitness: bool = lastAction == GraphRejectObservationAction
  val vectorDispatchWitness: bool = lastAction == VectorDispatchAction
  val vectorApplyWitness: bool = lastAction == VectorApplyAction
  val vectorResponseLossWitness: bool = lastAction == VectorResponseLossAction
  val vectorResponseObservationWitness: bool = lastAction == VectorResponseObservationAction
  val graphRemoveWitness: bool = lastAction == GraphRemoveAction
  val frontierDispatchWitness: bool = lastAction == FrontierDispatchAction
  val frontierApplyWitness: bool = lastAction == FrontierApplyAction
  val frontierResponseLossWitness: bool = lastAction == FrontierResponseLossAction
  val frontierResponseObservationWitness: bool =
    lastAction == FrontierResponseObservationAction
  val subjectClockGcWitness: bool = lastAction == SubjectClockGcAction
  val restartWitness: bool = lastAction == RestartAction

  val routerOwnershipPartition: bool = and {
    routerState.intents.forall(current => and {
      ROUTER_OPERATIONS.contains(current.operation),
      current.operation.stamp <= routerState.allocatedThrough,
    }),
    routerState.intents.forall(left =>
      routerState.intents.forall(right =>
        left.operation.stamp != right.operation.stamp or left == right
      )
    ),
  }

  val frontierDoesNotPassOutstanding: bool =
    routerState.intents.forall(current =>
      or {
        current.phase == AwaitingFrontier,
        current.operation.stamp >
          routerWatermarkForLane(vectorState, current.operation.lane),
      }
    )

  // Removing a subject clock is unsafe while an older exact Router operation
  // remains durable and may still reach Vector after Graph validation or replay.
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
    hasIntent(routerState, M10_UPSERT),
  }

  val staleResurrectionWitness: bool = and {
    policy == MaxAcknowledged,
    vectorState.targetClock == Live(10),
    vectorState.latestTargetRemoveStamp == 11,
  }

  val graphRejectionResolvedWitness: bool = and {
    lastAction == GraphRejectObservationAction,
    routerOwnershipPartition,
  }

  val partialPrefixRetainedWitness: bool = and {
    lastAction == VectorResponseObservationAction,
    hasIntentInPhase(routerState, M10_UPSERT, AwaitingFrontier),
    hasIntentInPhase(routerState, M12_UPSERT, AwaitingVector),
  }

  val laneIsolationWitness: bool = and {
    policy == Contiguous,
    hasIntent(routerState, M10_UPSERT),
    vectorState.primaryRouterWatermark <= 9,
    vectorState.secondaryRouterWatermark >= 14,
  }

  action step: bool = {
    nondet operation = ROUTER_OPERATIONS.oneOf()
    nondet otherOperation = ROUTER_OPERATIONS.oneOf()
    nondet lane = LANES.oneOf()
    nondet applied = Set(1, 2).oneOf()
    any {
      admitRouterOperation(operation),
      allocateUnrelatedM13,
      dispatchGraph(operation),
      graphAccept,
      graphReject,
      loseGraphResponse,
      observeGraphAccept,
      observeGraphReject,
      dispatchVectorBatch(List(operation)),
      dispatchVectorBatch(List(operation, otherOperation)),
      applyVectorPrefix(applied),
      loseVectorResponse,
      observeVectorResponse,
      applyGraphRemoveM11,
      dispatchRouterFrontier(lane),
      applyRouterFrontierMessage,
      loseFrontierResponse,
      observeFrontierResponse,
      gcTargetClock,
      restart,
    }
  }
}
