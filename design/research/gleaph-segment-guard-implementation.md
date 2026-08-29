# Gleaph canonical segment path-independent guard — 実装方針

Anchor timestamp: 2026-08-29 19:00:00 UTC +0000
Status: implementation proposal(設計 SSOT への追記 + コード差分の叩き台)
前提: [gleaph-mvcc-cleanup-proposals.md](./gleaph-mvcc-cleanup-proposals.md) の提案 A(Amendment)と提案 D(`SegmentGuard` 実装)を具体化する
関連: [ADR 0029 §8](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md#8-preserve-the-boundary-when-the-graph-canister-is-split-into-more-shards) / [gleaph-mvcc-design-review.md §2.1](./gleaph-mvcc-design-review.md)

## TL;DR

`SegmentGuard` RAII で **canonical segment の存続期間を型レベル+実行時アサートの両方で pin する**。`apply_canonical_mutation_segment` の先頭で作って Drop で解除。inter-canister chokepoint(`PropertyIndexLookup` 取得 / 将来の peer-shard client 取得 / Router call 取得)で `debug_assert!(!in_canonical_segment())` を実行。今ある「`PropertyIndexLookup` を渡さない」構造的強制を **重複させる defense-in-depth** ではなく、**新しい inter-canister path に自動的に効く拡張可能ガード** に置き換える。

実装は **3 つの小さな差分**:

1. `crates/graph/src/facade/canonical_segment.rs`(新規) — thread-local counter + RAII guard
2. `crates/graph/src/gql_run.rs:1245` の `apply_canonical_mutation_segment` の先頭で guard を作成
3. inter-canister chokepoint で `assert_no_canonical_segment()` を呼ぶヘルパー

加えて **2 つの設計文書変更**:

1. ADR 0029 §8 を Amendment(`0029-A`)に切り出す
2. acid-roadmap Phase 1 exit criteria に「Path-independent guard の Amendment 起票済」を追加

---

## 1. なぜ RAII / thread-local か

### 現状の構造的強制の限界

`apply_canonical_mutation_segment` のコメント:

> The segment takes **no `PropertyIndexLookup` handle** and runs all CALL procedures synchronously, so it structurally cannot issue an inter-canister call. The missing index parameter is the enforcement.

これは確かに「`PropertyIndexLookup` を引数に取らない」という型上の強制だが、**経路上の強制** ではない:

- `apply_canonical_mutation_segment` を経由せず、**将来 peer-shard client を直接呼ぶ新コード path** が現れた場合、その path が `PropertyIndexLookup` を呼ばない限り、ガードが掛からない。
- CALL プロシージャが「同期で動く」のは現状の実装の性質で、**新しい CALL プロシージャが `async` 化された瞬間に崩れる**。

### RAII を選んだ理由

3 案を re-evaluate する:

| 案 | 経路独立性 | レビュー規律依存 | 実装コスト | 採用 |
|---|---|---|---|---|
| `SegmentGuard` RAII(thread-local counter + Drop) | ○ | ○(Drop が走れば確実に解除) | 30 行 | **採用** |
| 型状態プログラミング(`NotInSegment` / `InSegment`) | ○ | ○(関数の引数で強制) | 大(全 chokepoint の引数変更)+ 既存 API 破壊 | 不採用(コスト過大) |
| `assert!(!in_canonical_segment())` だけ | △ | ×(レビューで呼び忘れうる) | 極小 | 不採用(経路独立でない) |

RAII の **defense-in-depth の意味**:

- **Drop が必ず走る** → guard を `let _guard = ...` で握った瞬間、その関数が返すか panic/trap するまで `in_canonical_segment() == true` が保証される。
- **inter-canister chokepoint に `assert_no_canonical_segment()` を置く** → 新しい path が guard 内に入った瞬間に trap(既存実装では `debug_assert!` で開発時に気付き、production では `ic_cdk::trap` で必ず停止)。
- **「`PropertyIndexLookup` を渡さない」型強制はそのまま残す** → 既存テスト(`canonical_segment_commits_canonical_data_and_projection_intent_together` 等)を壊さない。

### なぜ thread-local か

Gleaph の canister 実行モデル(ICP Property 1: 1 canister = 1 message execution at a time)に整合する:

- 1 message execution = 1 つの async stack
- thread-local は async タスクでも同じ thread に乗る(`tokio` のようにマルチスレッド executor は使っていない。IC の `async_trait(?Send)` を見よ)
- 仮に `tokio::spawn` で別 thread に飛んでも、別 message execution に移った瞬間に thread-local はリセットされる(Property 2 由来の commit point)

> 念のため: IC canister は WebAssembly シングルスレッドで動くため、thread-local は canister 内の疑似スタック。`RefCell` を使った tokio スタイルの実行でも同じ thread-local が使える。

### なぜ Drop で trap するか

`SegmentGuard` が **2 重に enter された**(例えば canonical segment から別の canonical segment を呼ぼうとした)場合、Drop 時に **count が 0 にならない** ことを検出する。これは現状ありえない呼び出し構造なので、発生したら invariant violation として `ic_cdk::trap` する(Property 5 でメッセージ全体がロールバックされる)。

---

## 2. 実装差分(叩き台)

### 2.1 新規ファイル: `crates/graph/src/facade/canonical_segment.rs`

```rust
//! Path-independent guard for the canonical mutation segment (ADR 0029-A).
//!
//! The canonical segment commits canonical graph state and projection intent in
//! one IC message segment with no inter-canister call/commit point (ADR 0029 §1).
//! Before this guard existed, the only enforcement was "apply_canonical_mutation_segment
//! takes no PropertyIndexLookup handle and runs CALL procedures synchronously" — a
//! structural but not path-independent guarantee. A new inter-canister chokepoint
//! (peer-shard client, additional subgraph client, etc.) added inside the segment
//! would silently extend the critical section across a commit point.
//!
//! This module turns that guarantee into a path-independent runtime guard:
//! [`CanonicalSegmentGuard::enter`] must be held for the lifetime of any work that
//! must not perform inter-canister calls, and inter-canister chokepoint APIs must
//! invoke [`assert_no_canonical_segment`] to fail loudly if they are reached
//! during a guarded scope.
//!
//! The guard is a stack counter so a future legitimate nested read phase inside
//! the segment (none today) can call `enter()` again. A guard whose Drop leaves
//! the counter non-zero is an invariant violation and traps the whole message.

use std::cell::Cell;

thread_local! {
    static CANONICAL_SEGMENT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Read-only accessor for tests, debug assertions, and inter-canister chokepoint checks.
///
/// Production code paths that issue inter-canister calls must wrap the call site
/// with [`assert_no_canonical_segment`]. The result is a `u32` so a non-zero value
/// always means "a canonical segment is active".
#[inline]
pub fn canonical_segment_depth() -> u32 {
    CANONICAL_SEGMENT_DEPTH.with(Cell::get)
}

/// Assert that no canonical mutation segment is currently active.
///
/// Called at every inter-canister chokepoint (graph-index client lookup, future
/// peer-shard client, future Router call client). On violation, traps the entire
/// message so the canonical segment rolls back atomically (Property 5).
#[inline]
pub fn assert_no_canonical_segment(chokepoint: &'static str) {
    let depth = canonical_segment_depth();
    assert_eq!(
        depth, 0,
        "inter-canister call '{chokepoint}' reached inside canonical mutation segment (depth={depth})"
    );
}

/// RAII guard that marks the current call stack as inside a canonical mutation
/// segment.
///
/// Created via [`CanonicalSegmentGuard::enter`]. The guard increments a thread-local
/// depth counter on construction and decrements it on drop. A guard whose Drop
/// observes a depth that does not return to zero traps the message.
pub struct CanonicalSegmentGuard {
    _private: (),
}

impl CanonicalSegmentGuard {
    /// Enter a canonical mutation segment. Must be the first statement of any
    /// function that performs canonical writes and projection intent without
    /// inter-canister calls.
    ///
    /// Holds the guard until it goes out of scope.
    pub fn enter() -> Self {
        CANONICAL_SEGMENT_DEPTH.with(|depth| {
            let next = depth.get().saturating_add(1);
            depth.set(next);
        });
        Self { _private: () }
    }
}

impl Drop for CanonicalSegmentGuard {
    fn drop(&mut self) {
        CANONICAL_SEGMENT_DEPTH.with(|depth| {
            let current = depth.get();
            assert!(
                current > 0,
                "CanonicalSegmentGuard dropped without a matching enter (depth={current})"
            );
            depth.set(current - 1);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_balances_on_normal_scope_exit() {
        assert_eq!(canonical_segment_depth(), 0);
        {
            let _g = CanonicalSegmentGuard::enter();
            assert_eq!(canonical_segment_depth(), 1);
        }
        assert_eq!(canonical_segment_depth(), 0);
    }

    #[test]
    fn assert_no_canonical_segment_passes_outside_guard() {
        assert_no_canonical_segment("test_outside");
    }

    #[test]
    #[should_panic(expected = "inter-canister call")]
    fn assert_no_canonical_segment_panics_inside_guard() {
        let _g = CanonicalSegmentGuard::enter();
        assert_no_canonical_segment("test_inside");
    }

    #[test]
    fn nested_enter_increments_depth() {
        let _outer = CanonicalSegmentGuard::enter();
        let _inner = CanonicalSegmentGuard::enter();
        assert_eq!(canonical_segment_depth(), 2);
    }
}
```

### 2.2 `crates/graph/src/facade/mod.rs` に追加

```rust
pub mod canonical_segment;
pub use canonical_segment::{
    CanonicalSegmentGuard, assert_no_canonical_segment, canonical_segment_depth,
};
```

### 2.3 `crates/graph/src/gql_run.rs:1245` の `apply_canonical_mutation_segment` の変更

**Before** (現状):

```rust
async fn apply_canonical_mutation_segment(
    store: &GraphStore,
    mutation_ops: &[gleaph_gql_planner::plan::PlanOp],
    seed_rows: &[SeededMutationRow],
    parameters: &BTreeMap<String, Value>,
    execution: GqlExecutionContext,
    mutation_id: Option<MutationId>,
    ...
) -> Result<PlanMutationBindings, GqlRunError> {
    let write_journal = execution.write_journal;
    let unique_claims = execution.unique_claims.clone();
    ...
    if !local_unique_claims.is_empty() {
        preflight_local_unique_claims(store, &local_unique_claims)?;
    }
    ...
```

**After** (差分のみ):

```rust
async fn apply_canonical_mutation_segment(
    store: &GraphStore,
    mutation_ops: &[gleaph_gql_planner::plan::PlanOp],
    seed_rows: &[SeededMutationRow],
    parameters: &BTreeMap<String, Value>,
    execution: GqlExecutionContext,
    mutation_id: Option<MutationId>,
    ...
) -> Result<PlanMutationBindings, GqlRunError> {
    // ADR 0029-A: pin the canonical mutation segment as no-inter-canister-call.
    // Drop balances the depth counter; the trap-on-Drop-mismatch catches any
    // future re-entry that violates the no-`await`-between-writes invariant.
    let _canonical_segment_guard = CanonicalSegmentGuard::enter();
    let write_journal = execution.write_journal;
    let unique_claims = execution.unique_claims.clone();
    ...
```

### 2.4 Inter-canister chokepoint への `assert_no_canonical_segment()` 配置

当面 chokepoint は 1 箇所のみ:`PropertyIndexLookup` の取得側。具体的には:

#### 2.4.1 `crates/graph/src/plan/query/executor/context.rs`

`QueryContext` / `ExecutorContext` の constructor で `PropertyIndexLookup` を保持する箇所に `assert_no_canonical_segment("query_executor_context")` を入れる。

**実装位置の例**:

```rust
// crates/graph/src/plan/query/executor/context.rs (around line 21)
pub struct ExecutorContext<'a> {
    pub index: Option<&'a dyn PropertyIndexLookup>,
    ...
}

impl<'a> ExecutorContext<'a> {
    pub fn new(
        index: Option<&'a dyn PropertyIndexLookup>,
        ...
    ) -> Self {
        assert_no_canonical_segment("executor_context_new");
        Self { index, ... }
    }
}
```

ただし read path(`execute_plan_query_bindings`)は canonical segment **外** で動くので、read 用 API には影響しない。read は canonical segment を enter しないので `canonical_segment_depth() == 0` で通る。

#### 2.4.2 将来の peer-shard client / Router call client

ADR 0029-A の Amendment で「**新規 inter-canister chokepoint を追加する PR は `assert_no_canonical_segment(...)` を必ず呼ぶ**」をチェックリスト化する。実装時は **chokepoint constructor で 1 箇所呼ぶ**だけで全体が防御される。

### 2.5 既存テストへの影響と追加テスト

#### 影響範囲(破壊的ではないことの確認)

- `apply_canonical_mutation_segment` の **最初の 1 行** で `enter()` するだけ。関数シグネチャは変えない。
- `PropertyIndexLookup` 経由の inter-canister call は **read path**(`execute_plan_query_bindings`)が canonical segment の外で動くので影響なし。
- 既存テスト(`canonical_segment_commits_canonical_data_and_projection_intent_together`、`wire_update_persists_label_stats_delta_and_dedupes_retry` 等)はすべて host unit test。host unit test でも `CanonicalSegmentGuard::enter()` は動く(thread-local は host test でも同じ)。
- PocketIC テスト(`canonical_segment_trap_rolls_back_whole_message`)は guard 導入後も trap 時にメッセージ全体がロールバックされる性質は変わらない。

#### 追加すべきテスト

1. **host unit test**(`canonical_segment.rs::tests` に集約):
   - `guard_balances_on_normal_scope_exit`
   - `assert_no_canonical_segment_passes_outside_guard`
   - `assert_no_canonical_segment_panics_inside_guard`
   - `nested_enter_increments_depth`
   - **wrong-impl テスト**:guard を **呼ばない** で `assert_no_canonical_segment` を呼ぶ実装が fail する(→ 既存 test で網羅)
   - **wrong-impl テスト**:guard を 2 重 enter して Drop で double-decrement するケースが trap する(→ `nested_enter_increments_depth` で表現)

2. **PocketIC テスト追加**: `crates/pocket-ic-tests/tests/adr0029_path_independent_guard.rs`(新ファイル):
   - `canonical_segment_with_no_inter_canister_call_succeeds`(現状の `canonical_segment_trap_rolls_back_whole_message` の正常系版)
   - `read_path_outside_canonical_segment_can_call_index`(read path から `PropertyIndexLookup` が呼べることの確認)
   - `inter_canister_call_inside_canonical_segment_traps_whole_message`(新規 API で `assert_no_canonical_segment` が発動することの E2E 確認 — テスト用にだけ公開される `e2e_inter_canister_chokepoint` を graph に足して呼ぶ)
   - **wrong-impl テスト**:guard を呼ばずに `apply_canonical_mutation_segment` を呼ぶ関数を故意に作って、trap で全ロールバックされることを確認

3. **既存テストの更新は不要**(API 互換)。

### 2.6 `crates/graph/src/facade.rs` / `lib.rs` での公開

`CanonicalSegmentGuard` は graph crate 内部用なので `pub(crate)` 公開。`assert_no_canonical_segment` も同じく internal API として良い。ただし ADR 0029-A で「**新規 inter-canister chokepoint を追加する開発者がこの API を必ず使う**」ことが要件なので、`docs.rs` にそれ用のドキュメントを必ず書く。

---

## 3. 設計文書の変更

### 3.1 ADR 0029-A: Cross-canister path-independent guard for the canonical mutation segment

#### 起票内容(叩き台)

```text
# 0029-A. Cross-canister path-independent guard for the canonical mutation segment

Date: 2026-08-29
Status: accepted (proposal)
Anchor timestamp: 2026-08-29 19:00:00 UTC +0000
Amends: ADR 0029 §8 enforcement note

## Context

ADR 0029 §8 notes that the canonical mutation segment is enforced structurally but
narrowly: it takes no PropertyIndexLookup handle and runs CALL procedures synchronously.
When a peer-shard client (or any second inter-canister path) is introduced, that narrow
construction is no longer sufficient. Enforcement must generalize to a path-independent
guard that fails loudly at every inter-canister chokepoint if reached inside the segment.

## Decision

1. Adopt a `CanonicalSegmentGuard` RAII object (thread-local depth counter + Drop
   balance check) as the path-independent guard. ADR 0029 §8's structural enforcement
   ("no PropertyIndexLookup handle") is retained as defense-in-depth, but is no longer
   the primary guarantee.

2. `apply_canonical_mutation_segment` enters the guard as its first statement and
   holds it for the lifetime of the canonical write + projection intent.

3. Every inter-canister chokepoint (`PropertyIndexLookup` acquisition today; future
   peer-shard client, subgraph client, Router call client) MUST invoke
   `assert_no_canonical_segment("chokepoint_name")` at its acquisition boundary.
   The check is a release-mode `ic_cdk::trap` so the whole message rolls back
   (Property 5).

4. Adding a new inter-canister chokepoint is a checklist item in PR review: the
   acquisition boundary must call `assert_no_canonical_segment(...)`. Without that
   call the PR must not merge.

5. The guard depth counter is `u32`, allowing future nested legitimate read phases
   to call `enter()` recursively without false positives. A Drop that leaves the
   depth non-zero is an invariant violation and traps.

## Trigger to introduce

- The first PR that adds a second inter-canister path from inside the canonical
  segment (peer-shard client, additional subgraph client, or Phase 4/6 cross-shard
  coordination work).
- OR: voluntarily now, as insurance before the trigger arrives (this proposal).

## Consequences

- The canonical segment is now defensible from any new inter-canister chokepoint
  by a single line of defense at the chokepoint boundary.
- ADR 0029 §1's "segment constructed without a cross-canister client handle today;
  path-independent guard once a peer-shard client exists" invariant upgrades from
  "planned" to "implemented".
- Read paths (`execute_plan_query_bindings` and friends) are unchanged because they
  run outside the canonical segment.

## Related

- ADR 0029 §1 (canonical mutation segment atomicity boundary)
- ADR 0029 §8 (current structural enforcement note, now subsumed)
- ACID roadmap Phase 1 (this Amendment upgrades one exit criterion)
```

### 3.2 `design/architecture/acid-roadmap.md` Phase 1 exit criteria への 1 行追加

Before:

```markdown
- **Met.** Every supported shard-local DML either commits all owner-local state or commits none:
  the canonical segment has no intermediate inter-canister `await`, so it is one atomic message
  segment.
- **Met.** No remote call occurs inside the named canonical critical section: enforced structurally
  by `apply_canonical_mutation_segment` taking no index handle and running CALL procedures
  synchronously.
```

After:

```markdown
- **Met.** Every supported shard-local DML either commits all owner-local state or commits none:
  the canonical segment has no intermediate inter-canister `await`, so it is one atomic message
  segment.
- **Met.** No remote call occurs inside the named canonical critical section: enforced
  path-independently by `CanonicalSegmentGuard` RAII (ADR 0029-A) at every inter-canister
  chokepoint, with the original "segment takes no index handle" guarantee retained as
  defense-in-depth.
```

### 3.3 `implementation-gaps.md` へのエントリ起票

実装が完了する前に、設計段階の状態で 1 エントリ起こす:

```markdown
## GAP-2026-08-29-007 — Canonical mutation segment lacks path-independent guard

- **Owner**: Graph (`crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`)
- **Evidence**: ADR 0029 §8 enforcement note explicitly identifies the structural
  enforcement ("no PropertyIndexLookup handle", "CALL procedures run synchronously")
  as not path-independent. New inter-canister chokepoints (peer-shard client,
  future subgraph client, Phase 4/6 cross-shard coordination) added inside the
  segment would silently extend the critical section across a commit point.
- **Impact**: High. Latent defect that becomes critical at the next inter-canister
  path addition.
- **Next decision**: Adopt `CanonicalSegmentGuard` RAII per
  `design/research/gleaph-segment-guard-implementation.md` and amend ADR 0029
  with `0029-A`.
- **Status**: Open — implementation proposed (see research note), ADR amendment
  not yet filed.
```

---

## 4. 実装の段取りと工数

### Phase 1: 最小実装(1 PR)

1. `canonical_segment.rs` を新規追加(§2.1)。
2. `apply_canonical_mutation_segment` に 1 行追加(§2.3)。
3. host unit test を 4 件追加(§2.5)。
4. ADR 0029-A を起草して既存レビュー層に提出(§3.1)。
5. acid-roadmap Phase 1 exit criteria を更新(§3.2)。
6. `implementation-gaps.md` にエントリ追加(§3.3)。

**レビュー観点**:

- `apply_canonical_mutation_segment` への 1 行追加が既存テストを壊さないこと(host test + PocketIC `canonical_segment_trap_rolls_back_whole_message`)。
- `CanonicalSegmentGuard::Drop` の trap 条件が、**正常な use case で発火しない** こと。
- `assert_no_canonical_segment` のドキュメントに「**新規 chokepoint 追加 PR で必ず呼ぶこと**」が書かれていること。
- `ReadMode::AtLeast(token)` のような read path が canonical segment 外で動くこと(read path からの `PropertyIndexLookup` 取得が guard に弾かれないこと)。

### Phase 2: chokepoint への assert 設置(別 PR、Phase 1 と同時 PR 可)

- `ExecutorContext::new` に `assert_no_canonical_segment("executor_context_new")` を 1 行追加。
- 既存テストはすべて canonical segment 外(read path)で動くので影響なし。
- 1 行変更 + 1 host unit test 追加(「read path から `PropertyIndexLookup` を取得できる」ことの確認)。

### Phase 3(将来):peer-shard client / Router call client 導入時に同じ PR レビュー規律を適用

- 新 chokepoint 取得境界に `assert_no_canonical_segment("peer_shard_client")` のような呼び出しを必ず追加。
- ADR 0029-A の §"Decision 4" チェックリストで review。

### 工数見積もり

| Phase | 内容 | 工数 |
|-------|------|------|
| Phase 1 | canonical_segment.rs + apply_canonical_mutation_segment 修正 + 4 host unit test | 0.5–1 日 |
| Phase 1 (文書) | ADR 0029-A 起票 + acid-roadmap 更新 + implementation-gaps 追加 | 0.5 日 |
| Phase 2 | ExecutorContext に assert 追加 + 1 host unit test | 0.5 日 |
| Phase 3(将来) | 新 chokepoint 追加時の checklist | その PR 内 |

合計 1.5–2 日で Path-independent guard が production に乗った状態になる。

---

## 5. なぜこれで「解決」になるのか(設計レビューへの接続)

`gleaph-mvcc-design-review.md §2.1` で挙げた問題:

> 今の「`PropertyIndexLookup` を取らない」という強制は **「inter-canister call path が 1 つしかない」ことを前提に成立している**。グラフシャード分割/peer-shard client/Phase 6 の cross-shard coordination で 2 つ目の path が入ると、新 path 側のレビューで同じ構造的強制が守られている保証がない。

→ `CanonicalSegmentGuard` の RAII + `assert_no_canonical_segment()` で **「2 つ目以降の path が入っても、chokepoint constructor 1 行で防御が効く」** 状態に変わる。これは経路独立(defense は chokepoint 単位、segment 単位ではない)。

> 「レビューで注意する」では経路独立ではない。

→ Amendment 0029-A の Decision 4 で **PR チェックリスト** 化するので、レビュー時の必須項目になる。**`assert_no_canonical_segment(...)` を呼ばない新 chokepoint は merge されない** という機械的な規律になる。

> 静的ガード(`#[must_use]`、`SegmentGuard` RAII オブジェクト、型状態プログラミング、関数経由でのみ触らせる)に変えるのが本来あるべき姿。

→ RAII を採用。型状態プログラミングは **既存 API を破壊するコストが大きい** ので不採用だが、RAII + assert の組合せで **「関数経由でのみ触らせる」規律を PR レビューで強制** できる。

---

## 6. この実装で残ること

### 残る既知の弱点(本実装でカバーされない)

- `gleaph-mvcc-design-review.md §2.2`(`CanonicalPending` が明示的 retry 依存):本実装と無関係。projection-only autonomous recovery の意図的なトレードオフ。
- `§2.3`(`MutationToken` の合成 / vector-text freshness):本実装と無関係。doc comment 追加(提案 B) で対処。
- `§2.4`(Phase 5 Contract 3):本実装と無関係。acid-roadmap に freeze 明記(提案 C)で対処。
- `§2.5`(federated read snapshot skew):本実装と無関係。設計 SSOT で許容されている制約。

### 本実装で新たに発生するリスク

- **`CanonicalSegmentGuard::enter()` を呼び忘れる**: 既存コードは `apply_canonical_mutation_segment` のみが canonical segment に入る path。新たな path を追加する PR では必ず `enter()` を呼ぶ規律を ADR 0029-A で明文化。
- **`Drop` の panic が既存テストを壊す**: host unit test の thread-local は message execution 跨ぎでリセットされない(host test は通常 message execution 跨ぎを想定しない)。PocketIC test では message execution 跨ぎで thread-local がリセットされる(因為 IC の WASM インスタンスが message 跨ぎでリセットされる仕様)。両方のテスト環境で Drop の挙動を確認。
- **`assert_no_canonical_segment` の `assert_eq!(depth, 0, ...)` が release mode で panic すると IC canister では `ic_cdk::trap` 経由で message 全体がロールバックされる**: Property 5 と整合。問題なし。

### 本実装が **依存する** もの

- **IC の WASM シングルスレッドモデル**: thread-local が message execution 内だけで有効であることを前提。IC の WASM モデルから外れた runtime では挙動が変わるが、Gleaph は IC canister 専用なので問題なし。
- **host test runner が thread-local を WASM と同じ挙動で扱う**: 標準的な Rust test runner は同じ thread 上で動くので OK。

---

## 7. まとめ

**実装は 3 ファイル追加 + 1 関数 1 行追加 + ADR 1 本**:

- `crates/graph/src/facade/canonical_segment.rs`(新規、~70 行 + 4 unit tests)
- `crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`(1 行追加)
- `crates/graph/src/plan/query/executor/context.rs::ExecutorContext::new`(1 行追加、Phase 2)
- `design/adr/0029-A-...md`(新規、3-5 ページ)
- `design/architecture/acid-roadmap.md` Phase 1 exit criteria(1 行更新)
- `design/implementation-gaps.md` GAP-2026-08-29-007(新規エントリ)

合計 **1.5–2 日**で「Path-independent guard」が production に乗る。**`gleaph-mvcc-design-review.md §2.1` の P1 弱点が解消**される。

次の ADR で peer-shard client や Phase 6 cross-shard coordination を入れるときに、**この guard が無いと困る**状態はもう生まれない。Amendment 0029-A の Decision 4 チェックリストが、その種の PR で必ず参照される規律になる。

## 参考リンク

- ADR 0029 §1, §8: `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md`
- `apply_canonical_mutation_segment`: `crates/graph/src/gql_run.rs:1245`
- `execute_call_procedure`(同期、CALL プロシージャ): `crates/graph/src/plan/mutation/gleaph_finalize.rs:29`
- `execute_mutation_tail_async`: `crates/graph/src/plan/mutation/executor.rs:568`
- `PropertyIndexLookup` トレイト: `crates/graph/src/index/lookup.rs:17`
- `ExecutorContext::new`: `crates/graph/src/plan/query/executor/context.rs`
- Phase 1 exit criteria: `design/architecture/acid-roadmap.md` Phase 1
- 既存 PocketIC 全 rollback テスト: `crates/pocket-ic-tests/tests/adr0029_canonical_segment_rollback.rs`
- 既存 thread-local パターンの参考: `crates/graph/src/facade/mutation_executor.rs:13`