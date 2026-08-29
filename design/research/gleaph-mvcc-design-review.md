# Gleaph のトランザクション境界設計レビュー

Anchor timestamp: 2026-08-29 18:55:00 UTC +0000
Status: review(設計 SSOT は ADR 0029 / ACID roadmap / ADR 0030。本レビューは問題点を網羅的に抽出し、優先度を付けた)
関連: [gleaph-mvcc-and-ic-atomicity.md](./gleaph-mvcc-and-ic-atomicity.md) / [ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md) / [ACID roadmap](../architecture/acid-roadmap.md) / [ADR 0030](../adr/0030-cross-shard-uniqueness-tcc-reservation.md)

## TL;DR

設計は **問題の輪郭が明確で、ICP 上の制約と整合した選択** をしている。「MVCC を導入しなかったこと」自体は問題ではなく、**「ICP の message atomicity を SSOT として使い、シャード間は saga + watermark でつなぐ」** という構成は境界が綺麗。ただしレビューすると以下 6 つの弱点が見える(うち 3 つは ADR 自身が「穴」と自己認識している)。

| # | 弱点 | 重要度 | 出典 |
|---|------|--------|------|
| 1 | 構造的強制(`PropertyIndexLookup` ハンドルを取らない)は経路独立ではない | P1 | ADR 0029 §8 自己認識 |
| 2 | Phase 4 の autonomous recovery が projection-only に限定され canonical 再 dispatch をしない結果、`CanonicalPending` の進行が「クライアントの明示的 retry」だけに依存する | P2 | ACID roadmap Phase 4 |
| 3 | `MutationToken` が watermark 集合であり snapshot timestamp ではないため、**複数 mutation をまたぐ read** ではクライアント側でトークン合成が必要 | P2 | ADR 0029 §5 |
| 4 | Phase 5 multi-DML の表現力制約(純 INSERT or 単一 anchor threaded 以外拒否)が大きい | P2 | ADR 0029 §6 |
| 5 | Federated read に共有 snapshot が無く、count と index が **それぞれ別の watermark** で評価される | P2 | ADR 0029 §5 |
| 6 | Routing lease と TTL eviction のレース(routing 進行中に lease 切れ) | P3 | ADR 0029 §4 |

これらは設計 SSOT ですでに文書化されているか、ADR に明示的に「意図的な制約」として記録されており、闇の問題ではない。が、**新規要件が持ち込まれたときに拡張できる形にしておく必要がある**(特に #1 と #3)。

---

## 1. 設計の Strong Points

### 1.1 SSOT がはっきりしている

- **原子性境界 = ICP の message atomicity**(Property 1+2+5)。これは Gleaph が選んだ SSOT であり、外のどこにも定義を散らしていない。
- **canonical owner = Graph shard**、**derived projections = graph-index / Router label-stats**、**saga owner = Router** という責任分担が ADR 0029 §1/§2/§4 で 1 か所に固定されている(SSOT, DRY 評価: ✓)。
- `MutationToken` / `ReadMode` / `MutationLifecyclePhase` の語彙は `gleaph_graph_kernel::plan_exec` に 1 箇所(`crates/graph-kernel/src/plan_exec.rs`)。Gleaph 固有の境界が一般 GQL クレートに漏れていない(gql / gql-planner を generic ISO GQL に保つ境界が守られている)。

### 1.2 ICP の message atomicity を構造的に利用

- `apply_canonical_mutation_segment` は `PropertyIndexLookup` ハンドルを取らない設計で「型上 inter-canister call を呼べない」状態を作っている(Encapsulation: ✓)。
- PocketIC E2E の `canonical_segment_trap_rolls_back_whole_message`(`crates/pocket-ic-tests/tests/adr0029_canonical_segment_rollback.rs`)がトラップ時の全ロールバックを証明。Property 5 を IC 実行モデルに任せている前提を破綻させない。

### 1.3 projection と canonical の語彙分離

- `MutationLifecyclePhase::ProjectionPending` という語を使って「canonical durable だが projection 遅延中」という状態を明示的に表現。これは MVCC の旧バージョン参照と区別するために重要な語彙。
- `RouterError::ProjectionLag` を stale 返却ではなく retryable にする選択(Property 4 のインターリーブと Property 6 の delivery 保証を正しく扱う)。

### 1.4 強プロトコルを「必要な invariant の名前がついたときだけ」導入する gate

- ADR 0029 §7: 全てのクロス shard 強プロトコルに対して ADR テンプレ(owning canonical state / staged storage / read visibility / timeout recovery / upgrade / retention / conflict semantics)を要求している。これは「後で MVCC を入れる」決断がアドホックにならないようにする保険。
- ADR 0030 の cross-shard uniqueness がその最初の実例。Try を Router-local CAS に閉じ込め、Confirm/Cancel も Router-local に閉じることで、ICP 上は「分散 commit」を避けて実装コストを限定している。

---

## 2. 既知の弱点(設計文書自身が穴と認識)

### 2.1 [P1] 構造的強制は経路独立ではない

ADR 0029 §8 が **明示的に予言** している:

> Today the boundary is enforced structurally but narrowly: the canonical mutation segment is constructed without a `PropertyIndexLookup` handle and runs `CALL` procedures synchronously, so it cannot reach the only existing inter-canister paths. **When a peer-shard client is introduced, that narrow construction is no longer sufficient on its own.** Enforcement must then generalize to a path-independent guard — assert "no canonical segment is active" at every inter-canister chokepoint (graph-index client, Router call, and any peer-shard client) — so a new call path added inside the segment fails loudly instead of silently extending the critical section across a commit point. This guard is expected when a second inter-canister path first appears (peer-shard client or the Phase 4/6 cross-shard coordination work), not before.
> — ADR 0029 §8 "Enforcement note"

何が問題か:
- 今の「`PropertyIndexLookup` を取らない」という強制は **「inter-canister call path が 1 つしかない」ことを前提に成立している**。グラフシャード分割/peer-shard client/Phase 6 の cross-shard coordination で 2 つ目の path が入ると、新 path 側のレビューで同じ構造的強制が守られている保証がない。
- 「レビューで注意する」では経路独立ではない。静的ガード(`#[must_use]`、`SegmentGuard` RAII オブジェクト、型状態プログラミング、関数経由でのみ触らせる)に変えるのが本来あるべき姿。
- 対策の ADR / Plan はまだない(`implementation-gaps.md` でも Open として未追跡。`GAP-2026-08-20-006 Router shard incarnation` とは別物)。

### 2.2 [P2] `CanonicalPending` の進行が明示的 retry だけ

ACID roadmap Phase 4 が意図的にそうしている:

> the background timer drives only the safe, idempotent half of recovery — projection/index convergence for sagas whose canonical writes are already durable. It deliberately does not re-dispatch canonical DML, because autonomous shard re-execution is the single operation that risks double-apply. **Unfinished canonical writes (`CanonicalPending`) are resumed by explicit idempotent retry, surfaced via `mutation_status`.**
> — ACID roadmap Phase 4

何が問題か:
- クライアントが retry を投げてくれない saga は投影が進まない。`mutation_status` を読んで retry を投げる責任は SDK / 運用者にある。
- これは設計としては **意図的なトレードオフ**(double-apply を避けるため)。ただし、retry を投げる主体が消えたときに「canonical durable だが projection 未適用」という宙ぶらりん状態を検知する仕組みは `mutation_status` だけで、**アラート昇格や強制再 dispatch のパスは設計されていない**。
- 長期放置された `CanonicalPending` saga は TTL 退避で消える(ADR 0025)。これは canonical write が durable である以上 **「消えても整合性は崩れない」** という意味では安全だが、「projection も永久に届かない」というプロダクト的問題はある。

### 2.3 [P2] `MutationToken` は snapshot timestamp ではない

ADR 0029 §5:

> A mutation token identifies the mutation and the shard-local projection watermarks required for read-your-writes. **The token is not a global MVCC snapshot timestamp.**

何が問題か:
- 単一 mutation の read-your-writes は完全にカバーされる。
- ただし **「mutation A と mutation B 両方の効果を見たい」** という要件には対応していない。クライアントは合成トークン(両方の `mutation_id` を含む)と、`label_stats_seq` の max を取り、`index_pending_min_mutation_id` の min を取る、というロジックを自分で組む必要がある。
- 理論上はクライアント側でできるが、SDK 側のヘルパ化が必要。設計 SSOT には「複数 mutation をまたぐ read のベストプラクティス」が書かれていない。

### 2.4 [P2] Phase 5 multi-DML の表現力制約

ADR 0029 §6:

> Contract 1 (one-shard atomic bundle), completely-new INSERT subset — implemented.
> Contract 1 (one-shard atomic bundle), anchored single-shard subset — implemented.
> Contract 2 (roll-forward bundle), single-anchor threaded subset — implemented.
> Staged distributed commit (contract 3) still reserved — its first named invariant (cross-shard uniqueness) is specified in ADR 0030 (accepted, implementation pending).
> — ADR 0029 §6

何が問題か:
- 「MATCH で 2 つ目の scan を含む multi-DML」と「独立した per-statement match」はまだ拒否される。GQL の表現力に対する強い制約で、ユーザーが複雑なビジネスロジックを書こうとすると「ここで atomically したい」が実現できない。
- 拒否の gate は 2 段(ad-hoc ingress と prepared registration)で同じ AST を見ている(DRY: ✓)。ただし「Contract 1/2 に入らないが、純粋なロジック変更で済む multi-DML」を救うパスは Contract 3(分散 staged commit)を要求するため、ユーザーに「これを使うには ADR を立てる必要」を伝えるコストが掛かる。

### 2.5 [P2] Federated read に共有 snapshot が無い

ADR 0029 Context:

> a read-only federated query has no shared snapshot timestamp across shards.

何が問題か:
- 複数シャードをまたぐ read-only federated query は、シャードごとに「いま見えている canonical/projection のスナップショット時刻」がバラバラで、**snapshot skew** を受け入れる契約になっている。
- 「ある時点で全シャードが consistent だった」という瞬間を観測できないので、`AtLeast(token)` を使っても **「指定した mutation 以前の状態」が保証されるのは 1 シャードずつ** で、クロスシャードの join 結果は skew を許容する。
- 設計文書では明示的な許容だが、OLTP 的に「厳密な point-in-time read」を期待するユーザーには surprise。

### 2.6 [P3] Routing lease と TTL eviction のレース

ADR 0029 §4 で `routing_lease_ns` / `ROUTING_LEASE_TTL_NS` が導入されている:

> A retry may reclaim a reservation whose lease has expired; this is safe because `routing_in_progress == true` implies the immutable envelope was not yet persisted and therefore no canonical write has happened.

何が問題か:
- lease 切れ → 再 routing → 新 envelope の間に、**古い routing が tenant principal を変えて dispatch した場合の競合** は「`routing_in_progress == true` なら envelope 未永続」という不変条件で防御されている、という理屈。
- ただし、これは「envelope 未永続」を巡る不変条件が **コードのレビューで守られているか** に依存し、経路独立ではない(2.1 と同根)。
- lease の精度と再 routing の所要時間の関係で「ぎりぎり lease 内に envelope 永続が完了」の race は理論上残る。タイムスタンプの単調性が IC の monotonic clock に依存する点をテストで縛っているかを確認すべき。

---

## 3. 構造的懸念(将来顕在化する弱点)

### 3.1 cluster-wide MVCC の拡張時に何が起きるか

ADR 0029 / ACID roadmap が「必要になるまで作らない」としている cluster-wide MVCC は、**導入する時点で大幅なリファクタリングが必要** になる:

- canonical storage に versioning を入れる(全 stable region の header 拡張)
- timestamp oracle を入れる(別途 canister が必要)
- prepared-state retention / read snapshot propagation / coordinator recovery をそれぞれ独立した不変条件として定義
- `MutationToken` の意味を「watermark 集合」から「snapshot timestamp」に変える
- `ReadMode::AtLeast(token)` を `ReadMode::AsOf(timestamp)` にリネーム/拡張

これは一気にやると破壊的。ADR 0029 §7 の ADR テンプレートで段階導入は可能なはずだが、**事前準備**(canonical storage の versioning hook を先に ADR で切っておくなど)が無いと、リファクタが「データ移行を伴う schema migration」になる。

### 3.2 index の floor が「first delivery + repair」だけを見ている

`index_pending_min_mutation_id`(`crates/graph/src/gql_run.rs` の ADR 0029 Phase 2 節参照)は `DerivedIndexOutbox` と `RepairJournal` の最小 mutation id を返す。これは「ordinary graph-index の projection lag」だけを測っており、**vector index / text index の freshness は測らない**。

何が問題か:
- 将来 vector search や全文検索の freshness barrier を要求するクライアントは、`AtLeast(token)` で「graph-index は最新」と見えても vector/text は見えない、という inconsistent な状態を読む可能性がある。
- これは今ドラフトの ADR が無いので将来課題だが、**`ReadMode::AtLeast` の意味は「graph-index の freshness」だけである**と明示しておく必要がある(現状の doc comment は部分的)。

### 3.3 phase 4 の autonomous recovery が bounded work を持つことの意味

recovery timer は bounded work を持つので「1 tick で全 recoverable saga を終わらせない」可能性がある。これは当然の選択だが、**retry back-off / per-tick scan budget の調整値** が設計文書に書かれていない(コード上は `crates/router/src/recovery.rs` にあるはず)。back-off が緩いと復旧が遅く、急ぎすぎると timer の instruction budget を圧迫する。

**検証したい点**: per-tick budget と back-off schedule の根拠が `recovery.rs` のコメントまたは plan として残されているか。

### 3.4 シャード内 `apply_canonical_mutation_segment` のテスト境界

PocketIC の `canonical_segment_trap_rolls_back_whole_message` が 1 つだけ存在することから、**canonical segment の atomicity 検証は PocketIC にしかできない** ことを前提としている。Host unit test は panic でロールバックしないため。これは妥当だが、**「test coverage = PocketIC 1 件」** は薄い:

- 異なる mutation 形(DELETE のみ、SET のみ、MATCH あり、unique claim あり)でトラップ位置を変えてロールバックを検証
- preflight 失敗と post-preflight 失敗(これは ADR 0029 §1 で「invariant violation → trap」と明示)を区別して、後者で recoverable Err を返していないことを担保
- 将来 peer-shard client が入ったとき、inter-canister call が path-independent guard に弾かれることを担保するテスト

これらはテスト拡充の余地あり。

---

## 4. 設計の境界面で見ると良い点(確認)

- `gleaph-gql` / `gleaph-gql-planner` は Gleaph 固有概念(`MutationToken`, `ReadMode`, `ShardId`)を持っていない(Encapsulation: ✓、`gql` / `gql-planner` 配下のソースに `MutationToken` / `ReadMode` を入れていないことを grep 確認済)。
- 設計境界は「一般 GQL → 言語 portable」「`gleaph_graph_kernel` → plan_exec の語彙 SSOT」「router / graph / graph-index → オーナー分離」と階層化されている(Gleaph architecture skill の Ownership Model と整合)。
- `Apply*` 関数の命名が一貫(`apply_canonical_mutation_segment` / `apply_propagation_intent` 系の語彙が読めばすぐ役割を連想させる)。
- `executor.rs` の `index: Option<&dyn PropertyIndexLookup>` という引数渡しは、**canonical segment で None を要求する形** に強制する良い API 形。ただし §2.1 の通り、**トレイトのメソッド経由でしか触れない構造にすべき**(今は `Option` を渡しているだけなので、レビューで `None` を禁じる規律が要る)。

---

## 5. 推奨(優先度順)

| 優先度 | 推奨 | 根拠 |
|--------|------|------|
| P1 | canonical segment の inter-canister 経路独立ガードを Plan/ADR で起こす | ADR 0029 §8 が穴として予告。peer-shard client や Phase 6 cross-shard coordination が近づく前に、型状態または `SegmentGuard` RAII で構造的に防ぐ。具体策は §2.1 参照 |
| P1 | `index_pending_min_mutation_id` の意味(ordinary graph-index のみ)と vector/text freshness の非対称性を canonical doc に明示 | §3.2 |
| P2 | multi-mutation read の合成レシピを SDK 側に持たせる前提を `MutationToken` doc に明記 | §2.3 |
| P2 | Phase 5 multi-DML の Contract 3(分散 staged commit)を、ADR 0030 の cross-shard uniqueness と一緒にでなく、**より一般的な multi-DML contract として再評価** する ADR を起こすか、「Contract 3 は出さない」と明示的に freeze する | §2.4 |
| P2 | PocketIC テストを「canonical segment のトラップ全 Rollback」1 件だけでなく、preflight 失敗 / post-preflight 失敗 / 異なる DML 形に増やす | §3.4 |
| P3 | recovery timer の per-tick budget / back-off schedule の根拠を `recovery.rs` のコメントまたは plan で残す | §3.3 |
| P3 | routing lease と envelope persist の race をカバーするテストの追加 | §2.6 |

---

## 6. 全体評価

設計は **問題の輪郭を自分で把握している** 良い設計。弱点の大半は ADR が「意図的なトレードオフ」と書いており、外部観測者が見ても隠れいていない。

ただし:

1. **経路独立ではない構造的強制**(§2.1)は 1 番目に潰すべき。Phase 6 の cross-shard coordination を入れたい瞬間に必ず問題になる。
2. **`MutationToken` の意味**(§2.3 / §3.2)は今ドキュメントに書かれていないが、ユーザーが multiple-mutation read や vector/text freshness barrier を求めたときに必ず surprise する。先回りで書いておくのが低コスト。
3. **Multi-DML Contract 3 の行く末**(§2.4)を freeze するか開くかをそろそろ決めておく。「ADR 0030 の最初のユーザが出てから考える」のままだと、後で必要になったとき ADR 一発では済まないリファクタになる。

それ以外は、設計 SSOT の境界が守られていて、テストの薄い領域も `canonical_segment_trap_rolls_back_whole_message` で証明されている。**設計自体に問題があるというより、「意図的な制約」+「まだ来ていない要件への備え」が明確かというレビュー** になっている。

## 参考リンク

- ADR 0029 §8(enforcement note): <https://github.com/.../blob/main/design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md#8-preserve-the-boundary-when-the-graph-canister-is-split-into-more-shards>(ローカル: `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md`)
- ACID roadmap Phase 4(scope decision): <https://.../design/architecture/acid-roadmap.md#phase-4-autonomous-federated-saga-recovery>
- `apply_canonical_mutation_segment`: `crates/graph/src/gql_run.rs:1245`
- `MutationToken` / `ReadMode`: `crates/graph-kernel/src/plan_exec.rs:919-948`
- Phase 4 recovery timer: `crates/router/src/recovery.rs`
- Phase 5 multi-DML gate: ADR 0029 §6
- PocketIC 全 rollback 証明: `crates/pocket-ic-tests/tests/adr0029_canonical_segment_rollback.rs::canonical_segment_trap_rolls_back_whole_message`