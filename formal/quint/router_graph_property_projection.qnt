// Bounded protocol model for one Router, one Graph shard, and one Property
// Index. The model deliberately keeps the durable projection queue as a List:
// queue order is the Graph-owned source of truth for work that still needs an
// acknowledgement. The Property Index projection is only a Set, so replay is
// idempotent without making the Set an owner of ordering.
module routerGraphPropertyProjection {
  type Mutation = str

  // Each mutation contributes two ordered postings. Keeping the mutation and
  // ordinal explicit makes a same-mutation partial prefix observable.
  type Posting = { mutation: Mutation, ordinal: int }

  // These tags are model instrumentation. They let witnesses identify that a
  // transition category was reached instead of inferring it from a state that
  // several different transitions could produce.
  type ActionTag =
    | InitAction
    | RouterDispatchAction
    | GraphCommitAction
    | RouterCompletionAction
    | RequestDeliveryAction
    | RecoveryAction
    | IndexApplyAction
    | IndexRejectAction
    | AppliedResponseLossAction
    | RejectedResponseLossAction
    | RestartAction
    | GraphRejectionObservationAction
    | GraphAcknowledgementAction

  // Volatile request delivery and response transport are separate from the
  // durable queue. A single delivery carries the current queue prefix (the
  // queue is finite in this bounded model), while an apply action chooses an
  // ordered prefix of that delivery.
  type Delivery = | DeliveryIdle | Delivered(List[Posting])
  type Response = | ResponseIdle | Applied(List[Posting]) | Rejected(List[Posting])

  pure val MUTATIONS = Set("m0", "m1")

  pure def posting(m: Mutation, ordinal: int): Posting = {
    mutation: m,
    ordinal: ordinal,
  }

  pure def postingsFor(m: Mutation): List[Posting] = if (m == "m0") {
    List(posting("m0", 0), posting("m0", 1))
  } else {
    List(posting("m1", 0), posting("m1", 1))
  }

  pure def asSet(xs: List[Posting]): Set[Posting] =
    xs.foldl(Set(), (seen, p) => seen.union(Set(p)))

  pure def listHas(xs: List[Posting], p: Posting): bool =
    xs.indices().exists(i => xs.nth(i) == p)

  pure def before(xs: List[Posting], first: Posting, second: Posting): bool =
    xs.indices().exists(i =>
      xs.indices().exists(j => and {
        i < j,
        xs.nth(i) == first,
        xs.nth(j) == second,
      })
    )

  pure def isPrefix(prefix: List[Posting], xs: List[Posting]): bool = and {
    prefix.length() > 0,
    prefix.length() <= xs.length(),
    prefix == xs.slice(0, prefix.length()),
  }

  pure def canonicalPostings(c: Set[Mutation]): Set[Posting] =
    c.map(m => asSet(postingsFor(m))).flatten()

  pure def mutationOf(p: Posting): Mutation = p.mutation

  pure def mutationRank(m: Mutation): int = if (m == "m0") { 0 } else { 1 }

  // Router-owned request state.
  var requested: Set[Mutation]
  var routerCompleted: Set[Mutation]

  // Graph canonical state and its durable ordered projection work. There is
  // no second durable intent collection: durableQueue owns pending work.
  var canonical: Set[Mutation]
  var durableQueue: List[Posting]

  // Property Index derived state. Sets intentionally discard delivery order;
  // apply ordering is enforced against durableQueue before insertion here.
  var projection: Set[Posting]

  // Volatile request/response transport state. Restart clears all of it.
  var delivery: Delivery
  var response: Response
  var restartPending: bool

  // Analysis-only transition instrumentation.
  var lastAction: ActionTag

  action init = all {
    requested' = Set(),
    routerCompleted' = Set(),
    canonical' = Set(),
    durableQueue' = List(),
    projection' = Set(),
    delivery' = DeliveryIdle,
    response' = ResponseIdle,
    restartPending' = false,
    lastAction' = InitAction,
  }

  // A statically disabled branch is used only to give match expressions a
  // uniform action effect. It is never selected by step and cannot stutter a
  // reachable execution.
  action disabled = all {
    false,
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = delivery,
    response' = response,
    restartPending' = restartPending,
    lastAction' = lastAction,
  }

  action routerDispatch(m: Mutation): bool = all {
    MUTATIONS.contains(m),
    not(requested.contains(m)),
    requested' = requested.union(Set(m)),
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = delivery,
    response' = response,
    restartPending' = restartPending,
    lastAction' = RouterDispatchAction,
  }

  // Graph's canonical mutation and its two durable postings are appended as
  // one transition. No await or Property Index response is represented inside
  // this atomic Graph-owned boundary.
  action graphCommit(m: Mutation): bool = all {
    requested.contains(m),
    not(canonical.contains(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical.union(Set(m)),
    durableQueue' = durableQueue.concat(postingsFor(m)),
    projection' = projection,
    delivery' = delivery,
    response' = response,
    restartPending' = restartPending,
    lastAction' = GraphCommitAction,
  }

  // Router completion is intentionally independent from Property Index
  // convergence. It requires canonical commit, but not an empty queue.
  action routerComplete(m: Mutation): bool = all {
    canonical.contains(m),
    not(routerCompleted.contains(m)),
    requested' = requested,
    routerCompleted' = routerCompleted.union(Set(m)),
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = delivery,
    response' = response,
    restartPending' = restartPending,
    lastAction' = RouterCompletionAction,
  }

  // A volatile request carries all currently durable work. Re-delivery after
  // response loss is therefore a real state change (Idle -> Delivered), not a
  // guaranteed-stutter duplicate action.
  action deliver: bool = all {
    not(restartPending),
    durableQueue.length() > 0,
    delivery == DeliveryIdle,
    response == ResponseIdle,
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = Delivered(durableQueue),
    response' = response,
    restartPending' = restartPending,
    lastAction' = RequestDeliveryAction,
  }

  // Recovery reconstructs volatile delivery from the durable queue after a
  // restart. It deliberately re-delivers already projected postings as well:
  // an applied response may have been lost after the Set write.
  action recover: bool = all {
    restartPending,
    durableQueue.length() > 0,
    delivery == DeliveryIdle,
    response == ResponseIdle,
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = Delivered(durableQueue),
    response' = response,
    restartPending' = false,
    lastAction' = RecoveryAction,
  }

  // The Property Index may apply only a non-empty ordered prefix of the
  // delivered request. Its response is emitted only after the Set update.
  action indexApplyPrefix(k: int): bool = match delivery {
    | Delivered(submitted) => all {
        response == ResponseIdle,
        k > 0,
        k <= submitted.length(),
        isPrefix(submitted, durableQueue),
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue,
        projection' = projection.union(asSet(submitted.slice(0, k))),
        delivery' = DeliveryIdle,
        response' = Applied(submitted.slice(0, k)),
        restartPending' = restartPending,
        lastAction' = IndexApplyAction,
      }
    | DeliveryIdle => disabled
  }

  // Rejection is also causally downstream of a delivered request. It does not
  // remove durable work or fabricate an acknowledgement.
  action indexReject: bool = match delivery {
    | Delivered(submitted) => all {
        response == ResponseIdle,
        submitted.length() > 0,
        isPrefix(submitted, durableQueue),
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue,
        projection' = projection,
        delivery' = DeliveryIdle,
        response' = Rejected(submitted),
        restartPending' = restartPending,
        lastAction' = IndexRejectAction,
      }
    | DeliveryIdle => disabled
  }

  // Response loss leaves both the durable queue and projection Set intact.
  // The next delivery/apply is a genuine idempotent replay because it creates
  // a new Applied response while the Set union remains unchanged.
  action loseAppliedResponse: bool = match response {
    | Applied(_) => all {
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue,
        projection' = projection,
        delivery' = delivery,
        response' = ResponseIdle,
        restartPending' = restartPending,
        lastAction' = AppliedResponseLossAction,
      }
    | ResponseIdle => disabled
    | Rejected(_) => disabled
  }

  action loseRejectedResponse: bool = match response {
    | Rejected(_) => all {
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue,
        projection' = projection,
        delivery' = delivery,
        response' = ResponseIdle,
        restartPending' = restartPending,
        lastAction' = RejectedResponseLossAction,
      }
    | ResponseIdle => disabled
    | Applied(_) => disabled
  }

  // Graph observes a rejection without removing any durable work. Retry is a
  // separate delivery action, so a rejected request cannot be mistaken for an
  // acknowledgement.
  action graphObserveRejected: bool = match response {
    | Rejected(_) => all {
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue,
        projection' = projection,
        delivery' = delivery,
        response' = ResponseIdle,
        restartPending' = restartPending,
        lastAction' = GraphRejectionObservationAction,
      }
    | ResponseIdle => disabled
    | Applied(_) => disabled
  }

  // Graph can remove exactly the applied response, and only when that response
  // is the current durableQueue prefix. The response and the same prefix are
  // removed together; a suffix cannot be acknowledged independently.
  action graphAcknowledge(count: int): bool = match response {
    | Applied(applied) => all {
        count == applied.length(),
        isPrefix(applied, durableQueue),
        requested' = requested,
        routerCompleted' = routerCompleted,
        canonical' = canonical,
        durableQueue' = durableQueue.slice(count, durableQueue.length()),
        projection' = projection,
        delivery' = delivery,
        response' = ResponseIdle,
        restartPending' = restartPending,
        lastAction' = GraphAcknowledgementAction,
      }
    | ResponseIdle => disabled
    | Rejected(_) => disabled
  }

  // Restart clears only volatile transport state. The queue, canonical Graph
  // state, and Property Index Set remain durable.
  action restart: bool = all {
    not(restartPending),
    or {
      delivery != DeliveryIdle,
      response != ResponseIdle,
    },
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableQueue' = durableQueue,
    projection' = projection,
    delivery' = DeliveryIdle,
    response' = ResponseIdle,
    restartPending' = true,
    lastAction' = RestartAction,
  }

  action step = {
    nondet m = oneOf(MUTATIONS)
    nondet k = oneOf(Set(1, 2, 3, 4))
    any {
      routerDispatch(m),
      graphCommit(m),
      routerComplete(m),
      deliver,
      recover,
      indexApplyPrefix(k),
      indexReject,
      loseAppliedResponse,
      loseRejectedResponse,
      graphObserveRejected,
      graphAcknowledge(k),
      restart,
    }
  }

  val canonicalOps = canonicalPostings(canonical)
  val queuedOps = asSet(durableQueue)

  // The queue is ordered; its projected prefix and unprojected pending suffix
  // must remain in that order even though projection itself is a Set.
  val orderedProjectionPendingSuffix = durableQueue ==
    durableQueue.select(p => projection.contains(p)).concat(
      durableQueue.select(p => not(projection.contains(p)))
    )

  val deliverySafe = match delivery {
    | DeliveryIdle => true
    | Delivered(submitted) => and {
        submitted.length() > 0,
        isPrefix(submitted, durableQueue),
      }
  }

  val responseCausalityAndExclusivity = and {
    // A request and its response occupy disjoint volatile phases.
    or {
      delivery == DeliveryIdle,
      response == ResponseIdle,
    },
    match response {
      | ResponseIdle => true
      | Applied(applied) => and {
          applied.length() > 0,
          isPrefix(applied, durableQueue),
          asSet(applied).subseteq(projection),
        }
      | Rejected(rejected) => and {
          rejected.length() > 0,
          isPrefix(rejected, durableQueue),
        }
    },
  }

  // Both postings of one mutation obey the same queue/projection ordering.
  // This makes an m0-o0 applied / m0-o1 pending state explicit rather than
  // collapsing the mutation to one Boolean.
  val sameMutationProjectionPrefix = MUTATIONS.forall(m => {
    val first = postingsFor(m).nth(0)
    val second = postingsFor(m).nth(1)
    and {
      or {
        not(projection.contains(second)),
        projection.contains(first),
      },
      or {
        not(listHas(durableQueue, first)),
        and {
          listHas(durableQueue, second),
          before(durableQueue, first, second),
        },
      },
    }
  })

  // Ownership and justification: every queue/projection entry is backed by
  // Graph canonical state, and canonical state is requested through Router.
  val ownershipAndJustification = and {
    canonical.subseteq(requested),
    queuedOps.subseteq(canonicalOps),
    projection.subseteq(canonicalOps),
  }

  // A canonical posting is either still in the durable queue or already in the
  // Set projection. In particular, response loss/restart cannot lose work.
  val noLostPendingWork = and {
    canonicalOps.exclude(projection).subseteq(queuedOps),
    canonicalOps == queuedOps.union(projection),
  }

  // The response cannot justify both outcomes, cannot coexist with delivery,
  // and an applied response is justified by the Set write it reports.
  val responseCausality = and {
    match response {
      | ResponseIdle => true
      | Applied(applied) => asSet(applied).subseteq(projection)
      | Rejected(rejected) => asSet(rejected).subseteq(queuedOps)
    },
    responseCausalityAndExclusivity,
  }

  // Graph acknowledgement is exact accounting for queue removal. Its action
  // guard additionally requires that the removed list is the current queue
  // prefix, while this invariant ensures no canonical posting vanishes.
  val exactAckPrefix = canonicalOps.exclude(queuedOps).subseteq(projection)

  // Completion is only justified by canonical commit. There is intentionally
  // no projection/queue condition here: Router may complete while work is
  // pending in Graph's queue.
  val routerCompletionIndependence = routerCompleted.subseteq(canonical)

  // This is a conceptual model rank derived from durableQueue order. It remains
  // distinct from the live exact Rust index_pending_min_mutation_id floor,
  // which is a fixed-index projection over tracked first-delivery outbox and
  // repair-journal source rows.
  val oldestDurableMutationRank = if (durableQueue.length() == 0) {
    2
  } else {
    mutationRank(mutationOf(durableQueue.head()))
  }

  val conceptualOldestDurableMutationRank = and {
    oldestDurableMutationRank >= 0,
    oldestDurableMutationRank <= 2,
  }

  val protocolSafety = and {
    ownershipAndJustification,
    noLostPendingWork,
    deliverySafe,
    orderedProjectionPendingSuffix,
    sameMutationProjectionPrefix,
    responseCausality,
    exactAckPrefix,
    routerCompletionIndependence,
    conceptualOldestDurableMutationRank,
  }

  // Positive witnesses for each major action category.
  val routerDispatchWitness = lastAction == RouterDispatchAction
  val graphCommitWitness = lastAction == GraphCommitAction
  val routerCompletionWitness = lastAction == RouterCompletionAction
  val requestDeliveryWitness = lastAction == RequestDeliveryAction
  val recoveryWitness = lastAction == RecoveryAction
  val indexApplyWitness = lastAction == IndexApplyAction
  val indexRejectWitness = lastAction == IndexRejectAction
  val appliedResponseLossWitness = lastAction == AppliedResponseLossAction
  val rejectedResponseLossWitness = lastAction == RejectedResponseLossAction
  val restartWitness = lastAction == RestartAction
  val graphRejectionObservationWitness =
    lastAction == GraphRejectionObservationAction
  val graphAcknowledgementWitness = lastAction == GraphAcknowledgementAction
}
