// Bounded protocol model for one Router, one Graph shard, and one Property Index.
// The model intentionally represents a posting by its mutation id. It is a
// protocol model, not a model of Candid, stable-memory bytes, or query plans.
module routerGraphPropertyProjection {
  type Mutation = str

  pure val MUTATIONS = Set("m0", "m1")

  // Router-owned request state.
  var requested: Set[Mutation]
  var routerCompleted: Set[Mutation]

  // Graph canonical state and its durable projection intent.
  var canonical: Set[Mutation]
  var durableIntent: Set[Mutation]

  // Property Index derived state and the durable acknowledgement observed by Graph.
  var projection: Set[Mutation]
  var acknowledged: Set[Mutation]
  var rejected: Set[Mutation]

  // Volatile delivery state is cleared by restart; durableIntent is not.
  var volatileInFlight: Set[Mutation]

  action init = all {
    requested' = Set(),
    routerCompleted' = Set(),
    canonical' = Set(),
    durableIntent' = Set(),
    projection' = Set(),
    acknowledged' = Set(),
    rejected' = Set(),
    volatileInFlight' = Set(),
  }

  action routerDispatch(m: Mutation): bool = all {
    not(requested.contains(m)),
    requested' = requested.union(Set(m)),
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
    volatileInFlight' = volatileInFlight,
  }

  // This is the critical atomic boundary: canonical state and the durable
  // projection intent are created together, without an await between them.
  action graphCommit(m: Mutation): bool = all {
    requested.contains(m),
    not(canonical.contains(m)),
    canonical' = canonical.union(Set(m)),
    durableIntent' = durableIntent.union(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
    volatileInFlight' = volatileInFlight,
  }

  // Router completion is deliberately independent from Property Index
  // convergence. This captures ADR 0024/0057's separate completion domains.
  action routerComplete(m: Mutation): bool = all {
    canonical.contains(m),
    routerCompleted' = routerCompleted.union(Set(m)),
    requested' = requested,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
    volatileInFlight' = volatileInFlight,
  }

  action deliver(m: Mutation): bool = all {
    durableIntent.contains(m),
    volatileInFlight' = volatileInFlight.union(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
  }

  // Property Index apply is idempotent and acknowledges the mutation only
  // after the posting is present.
  action indexApply(m: Mutation): bool = all {
    volatileInFlight.contains(m) or durableIntent.contains(m),
    projection' = projection.union(Set(m)),
    acknowledged' = acknowledged.union(Set(m)),
    rejected' = rejected.exclude(Set(m)),
    volatileInFlight' = volatileInFlight.exclude(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
  }

  // A rejection does not acknowledge or remove durable work.
  action indexReject(m: Mutation): bool = all {
    volatileInFlight.contains(m) or durableIntent.contains(m),
    rejected' = rejected.union(Set(m)),
    volatileInFlight' = volatileInFlight.exclude(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
  }

  // Graph removes the durable intent only after an explicit Property Index
  // acknowledgement. This is the acknowledged-prefix rule in miniature.
  action graphFinalize(m: Mutation): bool = all {
    or {
      and {
        m == "m0",
        acknowledged.contains(m),
      },
      and {
        m == "m1",
        acknowledged.contains(m),
        not(durableIntent.contains("m0")),
      },
    },
    durableIntent' = durableIntent.exclude(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected.exclude(Set(m)),
    volatileInFlight' = volatileInFlight.exclude(Set(m)),
  }

  // Duplicate delivery is an explicit transition so the test suite exercises
  // replay instead of relying only on set semantics in a single apply action.
  action duplicateDelivery(m: Mutation): bool = all {
    projection.contains(m),
    projection' = projection.union(Set(m)),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    acknowledged' = acknowledged,
    rejected' = rejected,
    volatileInFlight' = volatileInFlight,
  }

  action restart = all {
    volatileInFlight' = Set(),
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
  }

  action recover = all {
    volatileInFlight' = durableIntent,
    requested' = requested,
    routerCompleted' = routerCompleted,
    canonical' = canonical,
    durableIntent' = durableIntent,
    projection' = projection,
    acknowledged' = acknowledged,
    rejected' = rejected,
  }

  action step = {
    nondet m = oneOf(MUTATIONS)
    any {
      routerDispatch(m),
      graphCommit(m),
      routerComplete(m),
      deliver(m),
      indexApply(m),
      indexReject(m),
      graphFinalize(m),
      duplicateDelivery(m),
      restart,
      recover,
    }
  }

  // The lowest mutation id still represented by durable work. The finite
  // domain makes this a direct analogue of Graph's min pending watermark.
  val indexPendingMinMutationId = if (durableIntent.contains("m0")) {
    0
  } else if (durableIntent.contains("m1")) {
    1
  } else {
    2
  }

  val protocolSafety = and {
    // Graph is the sole owner of canonical state in this model.
    canonical.subseteq(requested),
    // Every canonical mutation remains recoverable until it is acknowledged.
    canonical.exclude(acknowledged).subseteq(durableIntent),
    durableIntent.subseteq(canonical),
    // A posting and its acknowledgement are justified by canonical state.
    projection.subseteq(canonical),
    acknowledged.subseteq(projection),
    // A rejection is a pending delivery outcome, never a terminal deletion.
    rejected.subseteq(durableIntent),
    // Router completion cannot precede canonical commit.
    routerCompleted.subseteq(canonical),
    indexPendingMinMutationId >= 0,
    indexPendingMinMutationId <= 2,
  }
}
