// Bounded Router-to-Graph CanonicalPending model for one ordered atomic_insert.
// This is a non-normative one-request, one-shard model. Production behavior is
// owned by the ADRs and the Rust/PocketIC comparisons documented in README.md.
module routerAtomicInsert {
  type Family = Edge | Vertex | Mixed
  type Fingerprint = F0 | F1
  type RequestToken = Request0 | Request1
  type Trigger = ExplicitSameKeyRetry | BackgroundRecovery
  type RouterPhase = CanonicalPending | CanonicalCommitted
  type EvidenceKind =
    | ExactActiveCompleted
    | Absent
    | QueryError
    | Retired
    | NotApplicable
    | NonCompleted
    | WrongFamily
    | WrongVersion
    | WrongFingerprint
    | WrongCount
    | WrongAllocation

  type JournalResponsePayload = {
    trigger: Trigger,
    evidence: EvidenceKind,
  }

  type Transport =
    | Idle
    | CanonicalRequest
    | CanonicalResponse
    | JournalQuery(Trigger)
    | JournalResponse(JournalResponsePayload)

  type Target = {
    family: Family,
    publicFingerprint: Fingerprint,
    graphFingerprint: Fingerprint,
    mutationId: int,
    targetShard: int,
    requestToken: RequestToken,
  }

  type Receipt = {
    family: Family,
    publicFingerprint: Fingerprint,
    graphFingerprint: Fingerprint,
    logicalCount: int,
    allocationCount: int,
  }

  type CommittedRecord = {
    target: Target,
    receipt: Receipt,
  }

  type RouterRecord =
    | NoRouterRecord
    | Pending(Target)
    | Committed(CommittedRecord)

  type GraphJournal = NoGraphJournal | ActiveCompleted(Receipt)
  type JournalFence =
    | NoJournalFence
    | ExplicitAbsentFence
    | BackgroundAbsentFence
    | QueryErrorFence
    | NonExactFence

  // Model-only before-image carried by actions whose contract is to preserve
  // Router and Graph durable owners. It is not production state.
  type OwnerSnapshot = {
    routerRecord: RouterRecord,
    graphJournal: GraphJournal,
    canonicalEffectCount: int,
    graphDispatchCount: int,
    journalFence: JournalFence,
    admissionTarget: Target,
  }

  type ActionTag =
    | InitAction
    | AdmitAction
    | DispatchAction
    | GraphCommitAction
    | LoseCanonicalResponseAction
    | PersistCanonicalResponseAction
    | QueryJournalAction
    | JournalResponseAction
    | AdoptExactReceiptAction
    | ExplicitAbsentRedispatchAction
    | ObserveBackgroundAbsentAction(OwnerSnapshot)
    | ObserveQueryErrorAction(OwnerSnapshot)
    | RejectNonExactEvidenceAction(OwnerSnapshot)
    | DuplicateGraphDispatchAction
    | RestartAction(OwnerSnapshot)
    | ExactReplayAction(OwnerSnapshot)
    | RejectConflictAction(OwnerSnapshot)

  pure val DEFAULT_TARGET: Target = {
    family: Edge,
    publicFingerprint: F0,
    graphFingerprint: F0,
    mutationId: 0,
    targetShard: 0,
    requestToken: Request0,
  }

  pure val DEFAULT_RECEIPT: Receipt = {
    family: Edge,
    publicFingerprint: F0,
    graphFingerprint: F0,
    logicalCount: 0,
    allocationCount: 0,
  }

  // Durable Router and Graph owners.
  var routerRecord: RouterRecord
  var graphJournal: GraphJournal
  var canonicalEffectCount: int
  var graphDispatchCount: int
  var journalFence: JournalFence

  // Model-only immutable before-image of the target persisted at admission.
  var admissionTargetSnapshot: Target

  // Volatile transport and model-only diagnostic instrumentation.
  var transport: Transport
  var lastAction: ActionTag

  action init = all {
    routerRecord' = NoRouterRecord,
    graphJournal' = NoGraphJournal,
    canonicalEffectCount' = 0,
    graphDispatchCount' = 0,
    journalFence' = NoJournalFence,
    admissionTargetSnapshot' = DEFAULT_TARGET,
    transport' = Idle,
    lastAction' = InitAction,
  }

  // A statically disabled branch gives match expressions a uniform effect.
  // It is never selected by step and cannot stutter a reachable execution.
  action disabled = all {
    false,
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = transport,
    lastAction' = lastAction,
  }

  pure def isPending(record: RouterRecord): bool = match record {
    | Pending(_) => true
    | _ => false
  }

  pure def targetOf(record: RouterRecord): Target = match record {
    | Pending(target) => target
    | Committed(committed) => committed.target
    | NoRouterRecord => DEFAULT_TARGET
  }

  pure def phaseOf(record: RouterRecord): RouterPhase = match record {
    | Committed(_) => CanonicalCommitted
    | _ => CanonicalPending
  }

  pure def isCommitted(record: RouterRecord): bool = match record {
    | Committed(_) => true
    | _ => false
  }

  pure def receiptOf(record: RouterRecord): Receipt = match record {
    | Committed(committed) => committed.receipt
    | _ => DEFAULT_RECEIPT
  }

  pure def exactReceipt(target: Target): Receipt = {
    family: target.family,
    publicFingerprint: target.publicFingerprint,
    graphFingerprint: target.graphFingerprint,
    logicalCount: if (target.family == Mixed) { 2 } else { 1 },
    allocationCount: if (target.family == Edge) { 0 } else { 1 },
  }

  pure def receiptMatches(target: Target, receipt: Receipt): bool = and {
    receipt == exactReceipt(target),
    receipt.family == target.family,
    receipt.publicFingerprint == target.publicFingerprint,
    receipt.graphFingerprint == target.graphFingerprint,
  }

  pure def journalReceipt(journal: GraphJournal): Receipt = match journal {
    | ActiveCompleted(receipt) => receipt
    | NoGraphJournal => DEFAULT_RECEIPT
  }

  pure def responseIs(
    current: Transport,
    trigger: Trigger,
    evidence: EvidenceKind,
  ): bool = match current {
    | JournalResponse(payload) => and {
        payload.trigger == trigger,
        payload.evidence == evidence,
      }
    | _ => false
  }

  pure def isNonExactEvidence(evidence: EvidenceKind): bool = match evidence {
    | Retired => true
    | NotApplicable => true
    | NonCompleted => true
    | WrongFamily => true
    | WrongVersion => true
    | WrongFingerprint => true
    | WrongCount => true
    | WrongAllocation => true
    | _ => false
  }

  pure def evidenceAllowed(journal: GraphJournal, evidence: EvidenceKind): bool =
    match evidence {
      | ExactActiveCompleted => journal != NoGraphJournal
      | Absent => journal == NoGraphJournal
      | _ => true
    }

  val currentOwners: OwnerSnapshot = {
    routerRecord: routerRecord,
    graphJournal: graphJournal,
    canonicalEffectCount: canonicalEffectCount,
    graphDispatchCount: graphDispatchCount,
    journalFence: journalFence,
    admissionTarget: admissionTargetSnapshot,
  }

  def coreOwnersMatch(before: OwnerSnapshot): bool = and {
    routerRecord == before.routerRecord,
    graphJournal == before.graphJournal,
    canonicalEffectCount == before.canonicalEffectCount,
    graphDispatchCount == before.graphDispatchCount,
    admissionTargetSnapshot == before.admissionTarget,
  }

  def allOwnersMatch(before: OwnerSnapshot): bool = and {
    coreOwnersMatch(before),
    journalFence == before.journalFence,
  }

  // Admission persists the complete target before any canonical dispatch.
  action admitPending(
    family: Family,
    publicFingerprint: Fingerprint,
    graphFingerprint: Fingerprint,
    mutationId: int,
    targetShard: int,
    requestToken: RequestToken,
  ): bool = {
    val admittedTarget: Target = {
      family: family,
      publicFingerprint: publicFingerprint,
      graphFingerprint: graphFingerprint,
      mutationId: mutationId,
      targetShard: targetShard,
      requestToken: requestToken,
    }
    all {
      routerRecord == NoRouterRecord,
      graphJournal == NoGraphJournal,
      canonicalEffectCount == 0,
      graphDispatchCount == 0,
      transport == Idle,
      journalFence == NoJournalFence,
      routerRecord' = Pending(admittedTarget),
      graphJournal' = graphJournal,
      canonicalEffectCount' = canonicalEffectCount,
      graphDispatchCount' = graphDispatchCount,
      journalFence' = journalFence,
      admissionTargetSnapshot' = admittedTarget,
      transport' = transport,
      lastAction' = AdmitAction,
    }
  }

  // Initial dispatch uses only the immutable target persisted by admission.
  action dispatchCanonical: bool = all {
    isPending(routerRecord),
    transport == Idle,
    canonicalEffectCount == 0,
    graphDispatchCount == 0,
    journalFence == NoJournalFence,
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount + 1,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = CanonicalRequest,
    lastAction' = DispatchAction,
  }

  // Graph persists its canonical effect and exact completed journal receipt
  // together, then exposes only a volatile response to Router. A duplicate
  // Graph dispatch takes the journal-first branch and returns the same receipt
  // without incrementing the canonical effect count.
  action graphCommit: bool = if (and {
    isPending(routerRecord),
    transport == CanonicalRequest,
    graphJournal == NoGraphJournal,
    canonicalEffectCount == 0,
  }) {
    all {
      graphJournal' = ActiveCompleted(exactReceipt(targetOf(routerRecord))),
      canonicalEffectCount' = canonicalEffectCount + 1,
      routerRecord' = routerRecord,
      graphDispatchCount' = graphDispatchCount,
      journalFence' = journalFence,
      admissionTargetSnapshot' = admissionTargetSnapshot,
      transport' = CanonicalResponse,
      lastAction' = GraphCommitAction,
    }
  } else if (and {
    isPending(routerRecord),
    transport == CanonicalRequest,
    graphJournal != NoGraphJournal,
    canonicalEffectCount == 1,
    receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
  }) {
    all {
      graphJournal' = graphJournal,
      canonicalEffectCount' = canonicalEffectCount,
      routerRecord' = routerRecord,
      graphDispatchCount' = graphDispatchCount,
      journalFence' = journalFence,
      admissionTargetSnapshot' = admissionTargetSnapshot,
      transport' = CanonicalResponse,
      lastAction' = GraphCommitAction,
    }
  } else {
    disabled
  }

  // The ordinary canonical response path persists the receipt directly. It
  // is intentionally separate from query-based journal adoption below.
  action persistCanonicalResponse: bool = all {
    isPending(routerRecord),
    transport == CanonicalResponse,
    graphJournal != NoGraphJournal,
    receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
    routerRecord' = Committed({
      target: targetOf(routerRecord),
      receipt: journalReceipt(graphJournal),
    }),
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = NoJournalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = Idle,
    lastAction' = PersistCanonicalResponseAction,
  }

  // A lost response removes only volatile transport state. The Graph journal
  // and canonical effect remain available for exact reconciliation.
  action loseCanonicalResponse: bool = all {
    isPending(routerRecord),
    transport == CanonicalResponse,
    graphJournal != NoGraphJournal,
    receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = Idle,
    lastAction' = LoseCanonicalResponseAction,
  }

  // Journal lookup is a query-only transition. It never changes dispatch or
  // canonical-effect accounting.
  action queryJournal(trigger: Trigger): bool = all {
    isPending(routerRecord),
    transport == Idle,
    graphDispatchCount > 0,
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = JournalQuery(trigger),
    lastAction' = QueryJournalAction,
  }

  // The response is a bounded semantic result, not a copy of the production
  // wire payload. Impossible exact/absent combinations are excluded by the
  // Graph journal ownership rule.
  action journalRespond(
    trigger: Trigger,
    evidence: EvidenceKind,
  ): bool = all {
    transport == JournalQuery(trigger),
    evidenceAllowed(graphJournal, evidence),
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = JournalResponse({
      trigger: trigger,
      evidence: evidence,
    }),
    lastAction' = JournalResponseAction,
  }

  // Exact active/completed evidence adopts the durable Graph receipt without
  // canonical redispatch, for either explicit retry or background recovery.
  action adoptExactReceipt(trigger: Trigger): bool = all {
    isPending(routerRecord),
    responseIs(transport, trigger, ExactActiveCompleted),
    graphJournal != NoGraphJournal,
    receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
    routerRecord' = Committed({
      target: targetOf(routerRecord),
      receipt: journalReceipt(graphJournal),
    }),
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = NoJournalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = Idle,
    lastAction' = AdoptExactReceiptAction,
  }

  // Only explicit same-key retry may redispatch after unambiguous absence.
  // The journal fence remains visible until the retried Graph commit.
  action explicitAbsentRedispatch: bool = all {
    isPending(routerRecord),
    responseIs(transport, ExplicitSameKeyRetry, Absent),
    graphJournal == NoGraphJournal,
    canonicalEffectCount == 0,
    graphDispatchCount > 0,
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount + 1,
    journalFence' = ExplicitAbsentFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = CanonicalRequest,
    lastAction' = ExplicitAbsentRedispatchAction,
  }

  // Background recovery observes absence but is forbidden from selecting the
  // canonical redispatch transition.
  action observeBackgroundAbsent: bool = all {
    isPending(routerRecord),
    responseIs(transport, BackgroundRecovery, Absent),
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = BackgroundAbsentFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = Idle,
    lastAction' = ObserveBackgroundAbsentAction(currentOwners),
  }

  action observeQueryError: bool = match transport {
    | JournalResponse(payload) => if (payload.evidence == QueryError) {
        all {
          routerRecord' = routerRecord,
          graphJournal' = graphJournal,
          canonicalEffectCount' = canonicalEffectCount,
          graphDispatchCount' = graphDispatchCount,
          journalFence' = QueryErrorFence,
          admissionTargetSnapshot' = admissionTargetSnapshot,
          transport' = Idle,
          lastAction' = ObserveQueryErrorAction(currentOwners),
        }
      } else {
        disabled
      }
    | _ => disabled
  }

  action rejectNonExactEvidence: bool = match transport {
    | JournalResponse(payload) => if (isNonExactEvidence(payload.evidence)) {
        all {
          isPending(routerRecord),
          routerRecord' = routerRecord,
          graphJournal' = graphJournal,
          canonicalEffectCount' = canonicalEffectCount,
          graphDispatchCount' = graphDispatchCount,
          journalFence' = NonExactFence,
          admissionTargetSnapshot' = admissionTargetSnapshot,
          transport' = Idle,
          lastAction' = RejectNonExactEvidenceAction(currentOwners),
        }
      } else {
        disabled
      }
    | _ => disabled
  }

  // A Graph-side duplicate re-enters the canonical handler with the stored
  // identity. The following graphCommit transition is journal-first replay.
  action duplicateGraphDispatch: bool = all {
    isPending(routerRecord),
    transport == Idle,
    graphJournal != NoGraphJournal,
    canonicalEffectCount == 1,
    receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount + 1,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = CanonicalRequest,
    lastAction' = DuplicateGraphDispatchAction,
  }

  // Restart clears only volatile transport. Durable Router and Graph owners
  // are copied unchanged.
  action restart: bool = all {
    transport != Idle,
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = Idle,
    lastAction' = RestartAction(currentOwners),
  }

  // An exact committed replay returns the already stored receipt. The model
  // records the observation without inventing a second response channel.
  action exactReplay: bool = all {
    isCommitted(routerRecord),
    transport == Idle,
    receiptMatches(targetOf(routerRecord), receiptOf(routerRecord)),
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = transport,
    lastAction' = ExactReplayAction(currentOwners),
  }

  // A changed family or public fingerprint is a conflict, not a new target.
  action rejectConflict(
    conflictingFamily: Family,
    conflictingFingerprint: Fingerprint,
  ): bool = all {
    routerRecord != NoRouterRecord,
    transport == Idle,
    or {
      conflictingFamily != targetOf(routerRecord).family,
      conflictingFingerprint != targetOf(routerRecord).publicFingerprint,
    },
    routerRecord' = routerRecord,
    graphJournal' = graphJournal,
    canonicalEffectCount' = canonicalEffectCount,
    graphDispatchCount' = graphDispatchCount,
    journalFence' = journalFence,
    admissionTargetSnapshot' = admissionTargetSnapshot,
    transport' = transport,
    lastAction' = RejectConflictAction(currentOwners),
  }

  action step = any {
    // The fixed family/fingerprint choices keep sampling finite while still
    // exercising edge, vertex, and mixed receipt shapes.
    admitPending(Edge, F0, F0, 0, 0, Request0),
    admitPending(Vertex, F0, F0, 0, 0, Request0),
    admitPending(Mixed, F0, F0, 0, 0, Request0),
    dispatchCanonical,
    graphCommit,
    persistCanonicalResponse,
    loseCanonicalResponse,
    queryJournal(ExplicitSameKeyRetry),
    queryJournal(BackgroundRecovery),
    journalRespond(ExplicitSameKeyRetry, ExactActiveCompleted),
    journalRespond(ExplicitSameKeyRetry, Absent),
    journalRespond(ExplicitSameKeyRetry, QueryError),
    journalRespond(ExplicitSameKeyRetry, Retired),
    journalRespond(ExplicitSameKeyRetry, NotApplicable),
    journalRespond(ExplicitSameKeyRetry, NonCompleted),
    journalRespond(ExplicitSameKeyRetry, WrongFamily),
    journalRespond(ExplicitSameKeyRetry, WrongVersion),
    journalRespond(ExplicitSameKeyRetry, WrongFingerprint),
    journalRespond(ExplicitSameKeyRetry, WrongCount),
    journalRespond(ExplicitSameKeyRetry, WrongAllocation),
    journalRespond(BackgroundRecovery, ExactActiveCompleted),
    journalRespond(BackgroundRecovery, Absent),
    journalRespond(BackgroundRecovery, QueryError),
    journalRespond(BackgroundRecovery, Retired),
    journalRespond(BackgroundRecovery, NotApplicable),
    journalRespond(BackgroundRecovery, NonCompleted),
    journalRespond(BackgroundRecovery, WrongFamily),
    journalRespond(BackgroundRecovery, WrongVersion),
    journalRespond(BackgroundRecovery, WrongFingerprint),
    journalRespond(BackgroundRecovery, WrongCount),
    journalRespond(BackgroundRecovery, WrongAllocation),
    adoptExactReceipt(ExplicitSameKeyRetry),
    adoptExactReceipt(BackgroundRecovery),
    explicitAbsentRedispatch,
    observeBackgroundAbsent,
    observeQueryError,
    rejectNonExactEvidence,
    duplicateGraphDispatch,
    restart,
    exactReplay,
    rejectConflict(Vertex, F1),
    rejectConflict(Edge, F1),
  }

  val canonicalApplyAtMostOnce = and {
    canonicalEffectCount >= 0,
    canonicalEffectCount <= 1,
  }

  val canonicalJournalAtomicity = if (canonicalEffectCount == 0) {
    graphJournal == NoGraphJournal
  } else {
    and {
      canonicalEffectCount == 1,
      graphJournal != NoGraphJournal,
      receiptMatches(targetOf(routerRecord), journalReceipt(graphJournal)),
    }
  }

  // Every admitted record is represented by one immutable Target payload;
  // all later transitions copy it rather than reconstructing routing data.
  val immutableStoredTarget = match routerRecord {
    | NoRouterRecord => admissionTargetSnapshot == DEFAULT_TARGET
    | Pending(target) => target == admissionTargetSnapshot
    | Committed(committed) => and {
        committed.target == admissionTargetSnapshot,
        receiptMatches(committed.target, committed.receipt),
      }
  }

  val exactReceiptJustification = match routerRecord {
    | NoRouterRecord => true
    | Pending(_) => true
    | Committed(committed) => and {
        graphJournal != NoGraphJournal,
        receiptMatches(committed.target, committed.receipt),
        receiptMatches(committed.target, journalReceipt(graphJournal)),
      }
  }

  val phaseMonotonicity = match routerRecord {
    | NoRouterRecord => and {
        phaseOf(routerRecord) == CanonicalPending,
        canonicalEffectCount == 0,
      }
    | Pending(_) => phaseOf(routerRecord) == CanonicalPending
    | Committed(_) => and {
        phaseOf(routerRecord) == CanonicalCommitted,
        canonicalEffectCount == 1,
      }
  }

  val backgroundNeverCanonicalDispatches = match lastAction {
    | ObserveBackgroundAbsentAction(before) => and {
        coreOwnersMatch(before),
        journalFence == BackgroundAbsentFence,
        transport == Idle,
      }
    | _ => true
  }

  val explicitRedispatchRequiresAbsent = if (
    transport == CanonicalRequest
  ) {
    or {
      and {
        lastAction == DispatchAction,
        canonicalEffectCount == 0,
        journalFence == NoJournalFence,
      },
      and {
        lastAction == ExplicitAbsentRedispatchAction,
        canonicalEffectCount == 0,
        journalFence == ExplicitAbsentFence,
      },
      and {
        lastAction == DuplicateGraphDispatchAction,
        canonicalEffectCount == 1,
        graphJournal != NoGraphJournal,
      },
    }
  } else {
    true
  }

  val nonExactEvidenceFailsClosed = match lastAction {
    | ObserveQueryErrorAction(before) => and {
        coreOwnersMatch(before),
        isPending(routerRecord),
        journalFence == QueryErrorFence,
        transport == Idle,
      }
    | RejectNonExactEvidenceAction(before) => and {
        coreOwnersMatch(before),
        isPending(routerRecord),
        journalFence == NonExactFence,
        transport == Idle,
      }
    | _ => true
  }

  val restartPreservesDurableOwners = match lastAction {
    | RestartAction(before) => and {
        allOwnersMatch(before),
        transport == Idle,
      }
    | _ => true
  }

  val replayAndConflictIsolation = match lastAction {
    | ExactReplayAction(before) => and {
        allOwnersMatch(before),
        isCommitted(routerRecord),
        receiptMatches(targetOf(routerRecord), receiptOf(routerRecord)),
        transport == Idle,
      }
    | RejectConflictAction(before) => and {
        allOwnersMatch(before),
        routerRecord != NoRouterRecord,
        transport == Idle,
      }
    | _ => true
  }

  val protocolSafety = and {
    canonicalApplyAtMostOnce,
    canonicalJournalAtomicity,
    immutableStoredTarget,
    exactReceiptJustification,
    phaseMonotonicity,
    backgroundNeverCanonicalDispatches,
    explicitRedispatchRequiresAbsent,
    nonExactEvidenceFailsClosed,
    restartPreservesDurableOwners,
    replayAndConflictIsolation,
  }

  val admitWitness = lastAction == AdmitAction
  val dispatchWitness = lastAction == DispatchAction
  val graphCommitWitness = lastAction == GraphCommitAction
  val canonicalResponseLossWitness = lastAction == LoseCanonicalResponseAction
  val journalQueryWitness = lastAction == QueryJournalAction
  val exactReceiptAdoptionWitness = lastAction == AdoptExactReceiptAction
  val explicitAbsentRedispatchWitness =
    lastAction == ExplicitAbsentRedispatchAction
  val backgroundAbsentWitness = match lastAction {
    | ObserveBackgroundAbsentAction(_) => true
    | _ => false
  }
  val nonExactEvidenceWitness = match lastAction {
    | RejectNonExactEvidenceAction(_) => true
    | _ => false
  }
  val restartWitness = match lastAction {
    | RestartAction(_) => true
    | _ => false
  }
  val exactReplayWitness = match lastAction {
    | ExactReplayAction(_) => true
    | _ => false
  }
  val conflictWitness = match lastAction {
    | RejectConflictAction(_) => true
    | _ => false
  }
}
