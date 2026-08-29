# Gleaph の MVCC と Internet Computer アトミック性モデルの対応

Anchor timestamp: 2026-08-29 18:30:53 UTC +0000
Status: research(設計契約ではなく、Gleaph のトランザクション境界が ICP 実行モデルとどう整合しているかを一次資料に基づいて整理したメモ)
関連: [ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md) / [ACID and consistency roadmap](../architecture/acid-roadmap.md) / [ADR 0030](../adr/0030-cross-shard-uniqueness-tcc-reservation.md)

## TL;DR

Gleaph は **MVCC を採用していない**。代わりに ICP の「1 つの canister メッセージ実行がアトミック」という性質をそのまま使い、シャード内キャノニカル書き込みを 1 つのメッセージセグメントに閉じ込め、シャード間は **ロールフォワード saga + projection バリア + 必要に応じた強プロトコル** でつなぐモデルに揃えている。cluster-wide MVCC / timestamp oracle / general 2PC は **明示的に out of scope**(ADR 0029 §"Alternatives", ACID roadmap Phase 6)。

この設計は「古典的な MVCC の代替」ではなく、ICP の以下の性質(Property 1/2/5)を前提に成立している:

- **Property 1**: 1 canister につき 1 message execution のみが走る(シリアル&アトミック)。
- **Property 2**: `await` が inter-canister call 境界となり、前後で別メッセージ実行になる。
- **Property 5**: トラップ/パニックでその message execution 内の状態変更は破棄される。

Gleaph の canonical mutation segment は **Property 2 によって生まれるコミットポイントを敢えて 1 個所にも作らない** ことでアトミック性を確保している(`apply_canonical_mutation_segment` は `PropertyIndexLookup` ハンドルを取らず、`CALL` を同期実行する構造的強制)。

## 1. Gleaph に MVCC がないことの一次資料根拠

### ADR 0029(Accepted, 2026-08-20 改訂)

`design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` は Gleaph のトランザクション境界を定義する最上位の決定文書。代替案の節で明示的にクラスタ全体 MVCC を退けている:

> **Introduce cluster-wide MVCC and two-phase commit now — Rejected for the general path.** It would add versioned canonical storage, staged writes, a timestamp authority, prepared-state retention, read snapshot propagation, and coordinator recovery before a demonstrated product requirement justifies them.
> — ADR 0029 §"Alternatives considered"

§5 のリード・コンシステンシも MVCC を否定:

> A mutation token identifies the mutation and the shard-local projection watermarks required for read-your-writes. **The token is not a global MVCC snapshot timestamp.**
> — ADR 0029 §5

### ACID roadmap(2026-08-20)

`design/architecture/acid-roadmap.md` の Non-goals が同趣旨:

> Adding cluster-wide MVCC before a query or invariant requires it.
> — ACID roadmap "Non-goals"

Phase 6 の description:

> Cluster-wide MVCC, a timestamp oracle, and general two-phase commit remain out of scope until this gate is met.
> — ACID roadmap Phase 6

## 2. Gleaph のモデル:3 層トランザクション境界

### 2.1 シャードローカル(原子性境界 = 1 canister message segment)

「1 つの canonical mutation を 1 つのグラフシャードの inter-canister commit point を含まない message segment 内で実行する」が対応の原子境界(ADR 0029 §1)。実装は `crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`。構造的強制:

- `PropertyIndexLookup` ハンドルを取らない(inter-canister パスが型上閉じている)
- CALL プロシージャを同期実行(`#[query]` 化せず同一 segment 内)
- `unique_claims` の preflight を最初の canonical write より前に済ませる

Phase 1 で追加されたベンチ(`crates/graph/canbench_results.yml`):

- `bench_graph_canonical_segment_insert_vertex` — 572.41 K 命令
- `bench_graph_canonical_segment_insert_vertex_with_property` — 598.66 K 命令
- `bench_graph_canonical_segment_insert_edge` — 792.35 K 命令
- `bench_graph_canonical_segment_insert_bundle_4` / `_16` — 約 672 K / 1.32 M 命令(線形)

PocketIC の E2E ロールバック証明:`canonical_segment_trap_rolls_back_whole_message` が `MATCH (h) INSERT (:RollbackOrphan) DELETE h` の DELETE 段でトラップさせたとき、オーファンもマッチハブも残らないことを保証(Property 5 相当)。

### 2.2 クロスシャード(ロールフォワード saga)

複数シャードにまたがる federated 変更は **ロールフォワードの saga** で扱う(ADR 0029 §4、Phase 4)。

- 不変: 1 mutation id を 1 シャードに二重適用しない(`Graph mutation journal lookup` で lookup)
- クライアント mutation reservation で同一キーの冪等性を確保
- `RouterMutationRecord` の per-shard `completed` / `projection_advanced` を cursor として retry
- **projection-only autonomous recovery**: `ic-cdk-timers` で `ProjectionPending` のみ自走させる。canonical DML を timer から再 dispatch しないのは二重適用リスクのため(明示的 idempotent retry に残す)

### 2.3 読み取り整合性(ReadMode と MutationToken)

`crates/graph-kernel/src/plan_exec.rs` に定義された二段:

```rust
pub struct MutationToken {
    pub mutation_id: MutationId,
    pub shards: Vec<MutationTokenShard>,
}

pub enum ReadMode {
    #[default]
    Eventual,
    AtLeast(MutationToken),
}

pub struct MutationTokenShard {
    pub shard_id: ShardId,
    pub label_stats_seq: Option<ShardEventSeq>,
}
```

`Eventual` は projection の遅延を許容(従来挙動)。`AtLeast(token)` は

- 各 token shard の label-stats projection cursor が `label_stats_seq` に到達
- 各 token shard の `index_pending_min_mutation_id` が `mutation_id` を満たす

の双方を Router で gating してから実行。満たさない場合は **stale を返さず** retryable `RouterError::ProjectionLag` を返す(ADR 0029 §5, ACID roadmap Phase 3)。

`Canonical` モードは **ADR 0056 で削除済み**(実装されておらず wire 受付もしない)。所有者の scan を直接読む読み取りモードは別 ADR で後続追加される予定。

## 3. ICP 特性との整合(なぜこのモデルが動くか)

`https://docs.internetcomputer.org/references/message-execution-properties.md`(2026-08 取得)より:

| Property | 一行要約 | Gleaph での使い方 |
|---|---|---|
| 1 | 1 canister につき 1 message execution のみが同時実行される | シャード内のシリアル化は ICP 側で既に保証。Gleaph は intra-canister ロックを持たない |
| 2 | inter-canister call を `await` した前後で別 message execution になる | canonical mutation segment から `await` を取り除くことが **そのまま原子性境界の定義** になる |
| 5 | トラップ/パニックで message execution 内の変更は破棄 | pre-flight / canonical write / projection intent を 1 つの segment に閉じ込めれば、トラップで全巻き戻しになる(Host unit test では panic ロールバックしないので PocketIC が必須) |
| 3 | 成功したリクエストは送信順で実行される | graph mutation journal の順序保証と router saga の replay 順序の根拠 |
| 4 | 複数 message execution はインターリーブし得る | projection の watermark チェックと idempotent retry で吸収 |
| 6/7 | inter-canister call は最大 1 回 delivery、1 回 response | durable journal / repair journal / routing lease 設計の根拠 |
| 8/9 | bounded-wait call は `SYS_UNKNOWN` を返すことがある | bounded-wait を使う sagas での reject ハンドリング要件 |

つまり Gleaph は **ICP の message-level 原子性をそのまま使い、シャード間だけは eventual / saga / 必要時のみ TCC という分散システム古典の道具で組み立てる** という戦略を取っている。これは「MVCC を諦めた」のではなく、**ICP の message atomicity を SSOT として使う** 選択。

## 4. 強プロトコルが許容される場所(任意)

ADR 0029 §7: クロス shard の一意性制約 / クォータ / スキーマ公開 / compare-and-set のように **クロス shard の all-or-nothing が具体的に必要になった invariant のみ** に TCC / MVCC / staged commit を導入する。

最初のインスタンスは ADR 0030 の cross-shard uniqueness:

- **Try** = Router-local reservation table の stable CAS
- **Confirm/Cancel** = Router-local
- 分散 write 自体は Phase 4 saga のまま

ADR 0029 が要求する strong protocol の ADR テンプレート(owning canonical state / staged storage / read visibility / timeout recovery / upgrade / retention / conflict semantics)は ADR 0030 で満たされる。

## 5. 現状のギャップと既知の制約

- **Federated read には共有 snapshot がない**(ADR 0029 Context):複数シャードを跨ぐ read-only federated query はそれぞれの projection の現時刻で読まれる。
- **Multi-DML 制約**: federated で複数 top-level DML 文を含むプログラムは §6 gate に弾かれる(Contract 1/2 の pure-insert / single-anchor threaded を除く)。MATCH で 2 つ目の scan を含むものは依然拒否。
- **Canonical read 非実装**: ADR 0056 で削除済み。owner-side scan ルーティングが必要なら別 ADR で後続追加。
- **Host unit test 限界**: in-memory store は panic でロールバックしないので、canonical segment の atomicity 検証は PocketIC の `adr0029_canonical_segment_rollback` 系でしか完結しない。

## 6. 自分の言葉で言い直すと

Gleaph の「MVCC にあたる層」は次の 3 つに分解できる:

1. **シャード内アトミック性** = ICP message atomicity(Property 1+2+5)を直接借用。`apply_canonical_mutation_segment` が「`await` を中に持たない」ことを型と構造で強制する。
2. **シャード間合意** = ロールフォワード saga(`RouterMutationRecord` + per-shard projection cursor + idempotent retry + projection-only 自動 recovery)。古典的な distributed commit の代替。
3. **read-your-writes** = `MutationToken`(per-shard `label_stats_seq` + graph-index の `index_pending_min_mutation_id`)を `ReadMode::AtLeast` で gate。MVCC snapshot timestamp ではなく「自分が書いた mutation の投影 watermark」だけを保証する局所的なバリア。

「cluster-wide ACID」を欲しがる要件は ADR 0030 の TCC パターンで個別に拡張する。タイムスタンプオラクル / prepared state retention / coordinator recovery のような MVCC の重い機構は、**それを要求する具体的な不変条件が出てきてから ADR で立ち上げる**(ACID roadmap Phase 6)。

## versioning について

canonical storage は **複数 version を保持しない**(MVCC versioning は不採用)。`MutationToken` は per-shard watermark 集合で snapshot timestamp ではなく、ICP message atomicity によって time-travel read や multi-version CAS の必要性は現在の contract では発生しない。将来 cluster-wide point-in-time read や multi-version CAS を要求する Phase-6+ invariant が出てきた場合は、**その時点で専用 ADR を起こして versioning を導入する**。stable storage に version field を予約する pre-reservation は行わない(必要にならない可能性のあるコストを永遠に払い続けるため)。Layout versioning(magic \| layout_version ヘッダ)と logical versioning(canister upgrade 時の旧領域読み捨て)は [`stable-memory-inventory.md`](../storage/stable-memory-inventory.md) で引き続き管理される。

## 参考リンク(一次資料)

- ICP Message Execution Properties: <https://docs.internetcomputer.org/references/message-execution-properties.md>(Property 1, 2, 5)
- ICP Concepts / Execution layer: <https://docs.internetcomputer.org/concepts/protocol/execution.md>
- ICP Concepts / Orthogonal persistence: <https://docs.internetcomputer.org/concepts/orthogonal-persistence.md>
- ICP Security best practices / Inter-canister calls: <https://docs.internetcomputer.org/guides/security/inter-canister-calls.md>
- Gleaph ADR 0029: `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md`
- Gleaph ACID roadmap: `design/architecture/acid-roadmap.md`
- Gleaph ADR 0030(TCC の最初の実例): `design/adr/0030-cross-shard-uniqueness-tcc-reservation.md`
- 実装 SSOT: `crates/graph-kernel/src/plan_exec.rs`(`MutationToken` / `ReadMode` / `MutationLifecyclePhase`), `crates/graph/src/gql_run.rs`(`apply_canonical_mutation_segment`), `crates/router/src/gql.rs`(`read_barrier` 検査), `crates/router/src/recovery.rs`(projection-only 自走 recovery)