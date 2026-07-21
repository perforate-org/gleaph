# 一括（bulk）mutation 実行コスト最適化 調査・設計引継ぎレポート

Date: 2026-07-21  
Anchor timestamp: 2026-07-21 09:28:07 UTC +0000  
Status: 設計前調査完了 / 実装未着手

## 背景と目的

social-demo の seed ワークロード（`SOCIAL_DEMO_USER_SCALE=5`, `POST_SCALE=20`）を
`batch-instr-log` feature で計測した結果、Graph shard 内の mutation 実行が全体コストの
支配的ボトルネックになっている。

挿入順に意味がない種類の mutation については、**ADR 0045** が LARA 層での
計画的な一括配置（planned batch placement）により実書き込みを劇的に軽くする道筋を示している。
本レポートは、その設計を別エージェントに委ねるにあたり、
現時点の実測コスト、関係する ADR・コードパス、優先的に設計すべき箇所、
残された問いをまとめたものである。

---

## 計測環境

- ワークロード: social-demo seed
  - `SOCIAL_DEMO_USER_SCALE=5`
  - `POST_SCALE=20`
  - 生成规模: 140 users, 7,100 posts
- 計測対象: Graph canister 内部 instruction log
- 計測時刻: 2026-07-21 07:30 UTC
- 生ログ: `/private/tmp/graph_instr_log_all.txt`
  - 338,439 行
  - `execute_plan_update`: 7,162 回
  - `execute_plan_update_batch`: 106 回
  - `execute_plan_impl` フェーズ記録: 22,098 回
- ネットワーク（実行時）:
  - host: `http://localhost:32852`
  - router: `4caro-hl777-77775-aaaba-cai`
  - graph shard: `4qggx-l3777-77775-aaaca-cai`
  - index: `4fbx2-kt777-77775-aaabq-cai`

---

## コスト内訳

### `execute_plan_impl` トップレベル フェーズ

| phase | instructions | `execute_plan_impl` 内比率 |
|---|---:|---:|
| `run_wire_plans` | 63,313,770,668 | 75.9 % |
| `drain` | 19,963,013,057 | 23.9 % |
| `result_build` | 167,505,350 | 0.2 % |
| **合計** | **83,444,289,075** | 100 % |

### `run_wire_plans` 内の詳細ブレークダウン

`run_wire_plans` の中で大きいのは以下の機能単位（instruction log のラベル単位）。
これらは厳密な親子合計ではなく、各ラベルが計測区間の差分を捉えたものである。

| 計測ラベル | instructions | wire フェーズ中の目安比率 |
|---|---:|---:|
| `run_wire_plans_last_read_row_count / run_wire_plans` | 50,089,220,562 | 33.6 % |
| `run_wire_plans_inner / apply_canonical_mutation_segment` | 37,033,464,281 | 24.8 % |
| `apply_canonical_mutation_segment / execute_mutation_tail_async` | 30,666,642,904 | 20.6 % |
| `run_wire_plans_inner / read_phase_seed_rows` | 12,155,322,864 | 8.2 % |
| `run_wire_plans_last_read_row_count / journal_commit` | 7,225,439,056 | 4.8 % |
| `apply_canonical_mutation_segment / post_write_bookkeeping` | 6,017,856,694 | 4.0 % |
| `run_wire_plans_last_read_row_count / outbox_persist` | 5,314,896,335 | 3.6 % |
| `run_wire_plans_last_read_row_count / encode_rows` | 209,653,360 | 0.1 % |
| `read_phase_seed_rows / revalidation` | 460,847,881 | 0.3 % |

---

## 各フェーズの意味

- `run_wire_plans`  
  1 つの plan に対して read prefix、write path、結果 materialize、
  canonical mutation segment、journal/outbox 書き込みまでを一気に実行する大きなラッパー。
  現状は seed row ごとに scalar な GraphStore/LARA 操作を繰り返している。

- `apply_canonical_mutation_segment`  
  ADR 0029 §1 の shard-local canonical critical section。
  plan の mutation ops を seed row ごとに実行し、label-stats delta 意図を durable delta log に
  追記し、mutation journal を `Incomplete` にする。同一 message 内で await を挟まないため
  原子性が保証される。

- `execute_mutation_tail_async`  
  read phase で得られた seed row ごとに mutation tail（`PlanOp::InsertEdge` 等）を実行。
  1 行ごとに `apply_mutation_ops_async` を呼び出し、GraphStore の edge/property/inline value
  書き込みを行う。これが per-row コストの主要因。

- `read_phase_seed_rows`  
  plan の read prefix を property index を使って実行し、seed rows を生成。
  ADR 0046 Phase 2 で追加された canonical revalidation はここに含まれるが、
  コストは 0.46B と無視できる（0.31 %）。

- `drain`  
  `execute_plan_impl` 終了時に derived-index outbox を同期的に掃除（drain）する処理。
  instruction 的に 20B 消費しており、batch 化または lazy drain の余地がある。

- `outbox_persist` / `journal_commit` / `post_write_bookkeeping`  
  derived-index outbox の永続化、mutation journal の書き込み、
  hot-forward vertex や label stats などの後処理。bulk 化で一回の集約書き込みにできる余地あり。

---

## 関係する ADR と実装状況

| ADR | タイトル | 状態 | 本設計との関係 |
|---|---|---|---|
| **ADR 0045** | Unordered batch graph mutations and LARA placement planning | Proposed | **今回の最適化の中核**。計画的 slab/log 書き込み、動的 leaf 拡張、scalar fallback 等を定義。 |
| **ADR 0044** | Router bulk mutation key | Partially implemented | 同種 seed を 1 つの `MutationId` にまとめ、Router saga コストを削減。Graph 側は今でも scalar 実行。 |
| **ADR 0041** | Router-to-Graph batch mutation dispatch | Implemented | `execute_plan_update_batch` 経由で複数 operation を 1 回の Graph call にまとめる基盤。 |
| **ADR 0042** | Router dynamic instruction-budget batching | Implemented | `next_index` カーソルと動的 chunk サイズの基盤。 |
| **ADR 0046** | Multi-variable candidate seed relations | Proposed / 一部実装 | 多変数 seed の canonical revalidation は実装済みでコストは小さい。 |
| **ADR 0029** | Shard-local atomicity and cross-canister consistency | Implemented | canonical critical section の失敗原子性と mutation journal 所有の根幹。 |
| **ADR 0030** | Cross-shard uniqueness TCC reservation | Implemented | 現状、制約付き mutation は bulk パスから reject される。 |
| **ADR 0026** | Reverse adjacency differential repair | Implemented | 有向 edge の forward / reverse half、無向 edge の two-forward-half 不変条件。 |
| **ADR 0020** | Deferred maintenance timer drain | Implemented | slot 番号付けコンパクションは maintenance のみが実行する。batch 化はこれを侵さない。 |

---

## 優先的な設計対象

1. **LARA / GraphStore の unordered batch edge/vertex 挿入**
   - 最大のボトルネック。現状 `execute_mutation_tail_async` が per-row に scalar 書き込みを行う。
   - ADR 0045 の計画立案・容量予約・一括書き込み（plan / reserve / commit）を実装。
   - 対象: 有向 forward/reverse half、無向 two-forward-half、self-loop、parallel edge。

2. **derived-index outbox drain の batch 化 / lazy 化**
   - `drain` が `execute_plan_impl` ごとに 20B 消費。
   - 大きな bulk group に対して drain を毎回 fully 実行するのではなく、
     一定量ごとに batch 処理するか、deferred maintenance タイマーに委ねる設計を検討。

3. **journal / delta / outbox の per-group 集約**
   - `journal_commit` 7.2B、`post_write_bookkeeping` 6.0B、`outbox_persist` 5.3B。
   - 同じ bulk group 内の operation ごとに独立した durable event を出すのではなく、
     1 つの journal entry + 集約 delta log + 集約 outbox event にまとめる。

4. **plan / params の encoding 効率と Router chunking**
   - 現在 `graph_batch_chunk_len_for_bulk` は 2 MiB 安全上限で binary search する。
   - 同じ plan を持つ group では resolved label/property を 1 回だけ解決し、
     params を packed columnar blob にして転送バイト数を削減。
   - default response は count-only receipt にし、edge id が必要な場合は別ページング API。

---

## 設計エージェントに対する残された問い

### 基本設計

- Router-to-Graph の新しい一括 wire shape はどうするか？
  - 論理 edge 単位（forward/reverse/alias 等は含めない）で送信。
  - versioned Candid type / packed columnar blob の選択基準は canbench。
- LARA 層の `plan_batch_mutation` / `reserve_batch_mutation` / `commit_batch_mutation` API を
  どう定義し、失敗原子性を保つか？
  - reserve 後に canonical 書き込みが始まったら、以降の失敗は invariant violation として trap。
- 同じ bulk group 内で一部の edge が slab 入り、一部が overflow-log 入り、
  一部が rebalance が必要になる場合、どう 1 つの atomic chunk として扱うか？

### 互換性・所有権

- ADR 0044 の `MutationId` 単位 bulk group と、ADR 0045 の計画的一括実行をどう統合するか？
  - 今は operation cursor (`next_index`) で逐次再開する。
  - 一括 batch 化後も、Graph 側で部分的な完了を durable に記録し、
    retry で正しく再開できる必要がある。
- 制約付き mutation（ADR 0030）は当面 bulk パスから reject する方針を維持するが、
  将来的に per-item claim を扱えるようになったときの設計をどう残すか？
- derived-index outbox / label stats projection / reverse adjacency (ADR 0026) は
  誰が、いつ、どの単位で emit するか？

### 性能・計測

- same-subnet 10 MiB 最適化は v1 後の追加とするか？
  - ADR 0045 は v1 は portable 2 MiB としている。
- canbench ベンチマークで比較すべき workload shape（directed fan-out/fan-in、undirected、
  self-loop、parallel edge、new/existing bucket、small spill、multi-block expand 等）。
- instructions per logical item、stable-memory reads/writes、relocation count、
  log occupancy、deferred maintenance コストを分離して計測。

---

## 重要なコードパス

| コンポーネント | ファイル | 該当関数・構造 |
|---|---|---|
| Graph plan 実行 | `crates/graph/src/gql_run.rs` | `run_wire_plans`, `run_wire_plans_inner`, `apply_canonical_mutation_segment`, `read_phase_seed_rows` |
| Mutation tail 実行 | `crates/graph/src/plan/mutation/executor.rs` | `execute_mutation_tail_async`, `apply_mutation_ops_async` |
| Graph batch entrypoint | `crates/graph/src/canister/handlers.rs` | `execute_plan_batch`, `detect_bulk_mutation_id`, `sync_drain_derived_index_outbox` |
| Edge 書き込み | `crates/graph/src/facade/store/edge_insert.rs`, `adjacency.rs` | scalar edge insert path |
| LARA 書き込み | `crates/ic-stable-lara/src/labeled/graph/insert.rs`, `bidirectional/deferred.rs` | bucket 検索・容量確保・slab/log 配置 |
| Vertex / property 書き込み | `crates/graph/src/facade/store/vertex.rs`, `vertex_labels.rs`, `properties.rs`, `vertex_properties.rs`, `edge_properties.rs` | scalar sidecar writes |
| Derived-index outbox | `crates/graph/src/index/repair_journal.rs`, `facade/store/derived_index_outbox.rs`, `index/pending.rs` | drain / emit |
| Router bulk dispatch | `crates/router/src/gql.rs` | `execute_bulk_group`, `execute_prepared_bulk_group`, `graph_batch_chunk_len_for_bulk` |

---

## 推奨する設計成果物

1. 新規 ADR または ADR 0045 の設計詳細追記
   - wire shape、plan/reserve/commit API、失敗原子性、retry semantics。
2. `design/storage/lara.md` / `lara-dgap-contract.md` 更新
   - pending-aware leaf placement、maintenance-only compaction 境界の明示。
3. `design/storage/labeled-edge-inline-values.md` 更新
   - payload slab/log batch 配置、mirrored update 動作。
4. `design/storage/bulk-ingest-finalize.md` 更新
   - planned direct batch placement と post-ingest maintenance の区別。
5. ベンチマーク計画
   - canbench 用の新規 benchmark ファイルと比較対象 scalar benchmark。

---

## 制約と不変条件（設計で絶対に崩さないこと）

- 有向 edge は正確に forward + reverse half を持つ（ADR 0026）。
- 無向 edge は 1 または 2 つの forward half（self-loop は 1 つ）。
- parallel edge の forward/reverse / canonical/alias 対応は論理 ordinal で結合。
- reserve 失敗や validation 失敗は canonical 状態を一切変えない。
- 最初の canonical commit write 以降の recoverable 失敗は無い（trap）。
- slot 番号付けコンパクションは maintenance のみ（ADR 0020）。
- 2 MiB portable request/response 上限を v1 で維持。
- 制約付き mutation は当面 bulk パスから fail-closed（ADR 0030）。

---

## 次のアクション

1. 本レポートをレビューし、設計エージェントへの委譲範囲を確定する。
2. 設計エージェントに ADR 0045 の詳細設計・API・ベンチマーク計画を作成させる。
3. 設計が出た段階で、`architecture-integrity`、`gleaph-architecture`、`adr-review`、
   `design-sync`、`benchmark` 各 skill を使ってレビューする。
4. 実装フェーズに入る前に、canbench ベースラインを取得しておく。
